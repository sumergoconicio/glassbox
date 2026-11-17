use crate::{
    Claim, InspectReport, Page,
    binary::{Source, be16, be32, slice},
    office, xml,
};
use std::{collections::BTreeSet, path::Path};

pub(crate) fn inspect(path: &Path, report: &mut InspectReport) {
    if let Err(error) = inner(path, report) {
        report.error("djvu", error);
    }
}

fn inner(path: &Path, report: &mut InspectReport) -> xml::Result<()> {
    let mut source = Source::open(path)?;
    let header = source.range(0, 16)?;
    if &header[..8] != b"AT&TFORM" {
        return Err("invalid DjVu IFF signature".into());
    }
    let size = u64::from(be32(&header, 8)?);
    let end = 12 + size;
    if size < 4 || end > source.len {
        return Err("truncated DjVu FORM".into());
    }
    let kind = &header[12..16];
    if !matches!(kind, b"DJVM" | b"DJVU") {
        return Err("unsupported DjVu FORM type".into());
    }
    office::identify(report, "djvu", "image/vnd.djvu", "DjVu document");
    report.format.disagreement = !report.format.extension.is_empty()
        && !matches!(report.format.extension.as_str(), "djvu" | "djv");
    let mut cursor = 16;
    let mut expected = None;
    let mut components = BTreeSet::new();
    let mut blocks = 0;
    if kind == b"DJVU" {
        page(&mut source, cursor, end, report)?;
        report.counts.pages = Some(1);
        return Ok(());
    }
    while cursor < end {
        blocks += 1;
        if blocks > 4096 {
            return Err("DjVu chunk-count cap exceeded".into());
        }
        let (id, next, len) = chunk(&mut source, cursor, end)?;
        if blocks == 1 && &id != b"DIRM" {
            return Err("DjVu document has no leading DIRM".into());
        }
        match &id {
            b"DIRM" => {
                if expected.is_some() {
                    return Err("duplicate DjVu directory".into());
                }
                let dir = source.range(cursor + 8, len as usize)?;
                let flags = *dir.first().ok_or("truncated DjVu directory")?;
                if flags & 0x7f != 1 {
                    return Err("unsupported DjVu directory version".into());
                }
                let count = be16(&dir, 1)? as usize;
                if count > 4096 {
                    return Err("DjVu directory component cap exceeded".into());
                }
                report.claims.push(Claim {
                    key: "directory_components".into(),
                    value: count.to_string(),
                    source: "bytes",
                });
                if flags & 0x80 == 0 {
                    report.error("djvu", "indirect DjVu page count unavailable: BZZ component flags are not decoded; external files are not followed");
                    return Ok(());
                }
                slice(&dir, 3, count * 4)?;
                let mut offsets = BTreeSet::new();
                for i in 0..count {
                    offsets.insert(u64::from(be32(&dir, 3 + i * 4)?));
                }
                if offsets.len() != count
                    || offsets.iter().any(|&offset| offset < next || offset >= end)
                {
                    return Err("invalid DjVu directory offsets".into());
                }
                expected = Some(offsets);
            }
            b"FORM" => {
                if len < 4 {
                    return Err("short DjVu component FORM".into());
                }
                let subtype = source.range(cursor + 8, 4)?;
                if matches!(subtype.as_slice(), b"DJVU" | b"DJVI" | b"THUM") {
                    components.insert(cursor);
                    if subtype == b"DJVU" {
                        page(&mut source, cursor + 12, cursor + 8 + len, report)?;
                    }
                }
            }
            _ => {}
        }
        cursor = next;
    }
    if expected.as_ref() != Some(&components) {
        return Err("DjVu directory disagrees with component FORM offsets".into());
    }
    report.counts.pages = Some(report.pages.len());
    Ok(())
}

fn chunk(source: &mut Source, offset: u64, end: u64) -> xml::Result<([u8; 4], u64, u64)> {
    if end.saturating_sub(offset) < 8 {
        return Err("truncated DjVu chunk header".into());
    }
    let bytes = source.range(offset, 8)?;
    let length = u64::from(be32(&bytes, 4)?);
    let next = offset
        .checked_add(8 + length + (length & 1))
        .ok_or("DjVu chunk overflow")?;
    if next > end {
        return Err("DjVu chunk exceeds parent FORM".into());
    }
    Ok((bytes[..4].try_into()?, next, length))
}

fn page(source: &mut Source, start: u64, end: u64, report: &mut InspectReport) -> xml::Result<()> {
    if report.pages.len() >= 4096 {
        return Err("DjVu page-count cap exceeded".into());
    }
    let (id, mut cursor, len) = chunk(source, start, end)?;
    if &id != b"INFO" || !(5..=10).contains(&len) {
        return Err("DjVu page has no valid leading INFO".into());
    }
    let info = source.range(start + 8, len as usize)?;
    let width = u32::from(be16(&info, 0)?);
    let height = u32::from(be16(&info, 2)?);
    if width == 0 || height == 0 {
        return Err("invalid DjVu page dimensions".into());
    }
    let dpi = if info.len() >= 8 {
        u16::from_le_bytes(info[6..8].try_into()?)
    } else {
        300
    };
    let major = info.get(5).copied().unwrap_or(0);
    report.pages.push(Page {
        index: report.pages.len() + 1,
        source_part: format!("FORM@{}", start - 12),
        width: Some(width),
        height: Some(height),
        dpi: Some(dpi),
        version: Some(format!("{major}.{}", info[4])),
    });
    let mut blocks = 0;
    let mut compressed_text = false;
    while cursor < end {
        blocks += 1;
        if blocks > 4096 {
            return Err("DjVu page chunk-count cap exceeded".into());
        }
        let (id, next, _) = chunk(source, cursor, end)?;
        compressed_text |= &id == b"TXTz";
        cursor = next;
    }
    if compressed_text && !report.errors.iter().any(|e| e.stage == "djvu_text") {
        report.error(
            "djvu_text",
            "TXTz text unavailable: BZZ decoding is out of scope",
        );
    }
    Ok(())
}
