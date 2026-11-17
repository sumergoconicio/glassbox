#![cfg(all(feature = "parquet-preview", not(target_arch = "wasm32")))]

use litx_inspect::{InspectOptions, inspect};
use parquet_format_safe::{
    CompressionCodec, FileMetaData,
    thrift::protocol::{TCompactInputProtocol, TCompactOutputProtocol},
};
use parquet_reader::{
    data_type::Int64Type,
    file::{properties::WriterProperties, writer::SerializedFileWriter},
    schema::parser::parse_message_type,
};
use std::{path::PathBuf, sync::Arc};

fn seed() -> (Vec<u8>, FileMetaData) {
    let schema = Arc::new(parse_message_type("message rows { REQUIRED INT64 id; }").unwrap());
    let mut writer =
        SerializedFileWriter::new(Vec::new(), schema, Arc::new(WriterProperties::default()))
            .unwrap();
    let mut group = writer.next_row_group().unwrap();
    let mut column = group.next_column().unwrap().unwrap();
    column
        .typed::<Int64Type>()
        .write_batch(&[1], None, None)
        .unwrap();
    column.close().unwrap();
    group.close().unwrap();
    let bytes = writer.into_inner().unwrap();
    let footer_size =
        u32::from_le_bytes(bytes[bytes.len() - 8..bytes.len() - 4].try_into().unwrap()) as usize;
    let footer_start = bytes.len() - 8 - footer_size;
    let metadata = FileMetaData::read_from_in_protocol(&mut TCompactInputProtocol::new(
        &bytes[footer_start..bytes.len() - 8],
        1 << 20,
    ))
    .unwrap();
    (bytes[..footer_start].to_vec(), metadata)
}

#[test]
fn unsafe_parquet_chunks_keep_schema_but_never_produce_a_row_preview() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests")
        .join(format!(
            "parquet-bounds-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&directory).unwrap();
    for (case, message) in [
        ("external", "external"),
        ("codec", "codecs"),
        ("page", "8 MiB"),
    ] {
        let (mut data, mut metadata) = seed();
        let chunk = &mut metadata.row_groups[0].columns[0];
        match case {
            "external" => chunk.file_path = Some("/not-opened/sensitive.parquet".into()),
            "codec" => chunk.meta_data.as_mut().unwrap().codec = CompressionCodec::GZIP,
            _ => {
                // DATA_PAGE, i32::MAX expanded bytes, zero compressed bytes.
                let header = b"\x15\x00\x15\xfe\xff\xff\xff\x0f\x15\x00\x00";
                data = b"PAR1".to_vec();
                data.extend_from_slice(header);
                let meta = chunk.meta_data.as_mut().unwrap();
                meta.data_page_offset = 4;
                meta.dictionary_page_offset = None;
                meta.total_compressed_size = header.len() as i64;
            }
        }
        let mut footer = Vec::new();
        metadata
            .write_to_out_protocol(&mut TCompactOutputProtocol::new(&mut footer))
            .unwrap();
        data.extend_from_slice(&footer);
        data.extend_from_slice(&(footer.len() as u32).to_le_bytes());
        data.extend_from_slice(b"PAR1");
        let path = directory.join(format!("{case}.parquet"));
        std::fs::write(&path, data).unwrap();
        let report = inspect(&path, &InspectOptions::default());
        assert_eq!(report.parquet.unwrap().rows, 1, "{case}");
        assert!(report.dataset.is_none(), "{case}");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.stage == "parquet_preview" && e.message.contains(message)),
            "{case}: {:?}",
            report.errors
        );
    }
}
