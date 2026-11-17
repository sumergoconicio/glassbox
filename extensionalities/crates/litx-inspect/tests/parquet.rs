use litx_inspect::{InspectOptions, inspect};

#[cfg(feature = "parquet-preview")]
#[test]
fn parquet_preview_decodes_only_five_rows_across_row_groups() {
    use parquet_reader::{
        basic::Compression,
        data_type::{ByteArray, ByteArrayType, Int64Type},
        file::{properties::WriterProperties, writer::SerializedFileWriter},
        schema::parser::parse_message_type,
    };
    use std::sync::Arc;
    let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests")
        .join(format!(
            "parquet-rows-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("rows.parquet");
    let schema = Arc::new(
        parse_message_type("message rows { REQUIRED INT64 id; REQUIRED BINARY note (UTF8); }")
            .unwrap(),
    );
    let properties = Arc::new(
        WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .build(),
    );
    let mut writer =
        SerializedFileWriter::new(std::fs::File::create(&path).unwrap(), schema, properties)
            .unwrap();
    for values in [vec![1i64, 2, 3], vec![4i64, 5, 6, 7]] {
        let mut group = writer.next_row_group().unwrap();
        let mut column = group.next_column().unwrap().unwrap();
        column
            .typed::<Int64Type>()
            .write_batch(&values, None, None)
            .unwrap();
        column.close().unwrap();
        let notes = values
            .iter()
            .map(|id| {
                ByteArray::from(
                    if *id > 5 {
                        "DO_NOT_EXPORT".to_string()
                    } else {
                        format!("row {id}")
                    }
                    .as_str(),
                )
            })
            .collect::<Vec<_>>();
        let mut column = group.next_column().unwrap().unwrap();
        column
            .typed::<ByteArrayType>()
            .write_batch(&notes, None, None)
            .unwrap();
        column.close().unwrap();
        group.close().unwrap();
    }
    writer.close().unwrap();
    let r = inspect(&path, &InspectOptions::default());
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["parquet"]["rows"], 7);
    assert_eq!(v["dataset"]["headers"], serde_json::json!(["id", "note"]));
    assert_eq!(v["dataset"]["rows"].as_array().unwrap().len(), 5);
    assert_eq!(v["dataset"]["rows"][4], serde_json::json!([5, "row 5"]));
    assert_eq!(v["dataset"]["has_more"], true);
    assert!(!r.markdown.content.contains("DO_NOT_EXPORT"));
}

#[test]
fn parquet_inspection_reads_footer_schema_without_decoding_rows() {
    // Compact-Thrift FileMetaData: version 1, required schema root and two
    // primitive leaves, zero rows/row groups. No column data pages exist.
    let footer: &[u8] = b"\x15\x02\x19\x3c\x48\x06schema\x15\x04\x00\x15\x04\x25\x00\x18\x02id\x00\x15\x0c\x25\x02\x18\x04name\x00\x16\x00\x19\x0c\x00";
    let mut bytes = b"PAR1".to_vec();
    bytes.extend_from_slice(footer);
    bytes.extend_from_slice(&(footer.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"PAR1");
    let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests")
        .join(format!(
            "parquet-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("empty.parquet");
    std::fs::write(&path, bytes).unwrap();
    let r = inspect(&path, &InspectOptions::default());
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["parquet"]["rows"], 0);
    assert_eq!(v["parquet"]["row_groups"], 0);
    assert_eq!(v["parquet"]["leaf_columns"], 2);
    assert_eq!(v["parquet"]["columns"][0]["path"], "id");
    assert_eq!(v["parquet"]["columns"][0]["physical_type"], "INT64");
    assert_eq!(v["parquet"]["columns"][1]["path"], "name");
    assert_eq!(v["parquet"]["basis"], "footer_metadata");
    assert!(
        v["markdown"]["content"]
            .as_str()
            .unwrap()
            .contains("| id | INT64 |")
    );
}
