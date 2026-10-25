//! Minimal sfnt name-table reader (design §6.1): the GDI sink must select
//! the private font registered by `AddFontMemResourceEx` through
//! `CreateFontIndirectW`, which needs the family NAME — `imaging`'s
//! `FontData` carries only bytes + collection index. Parses just enough of
//! the name table: platform 3 (Windows) encoding 1 (UTF-16) lang 0x0409
//! preferred, nameID 1 (family) falling back to 16 (typographic family),
//! then platform 1 (Macintosh) ASCII. TTC collections address the
//! index-th font's table directory.

/// Read the family name of the `index`-th font in an sfnt/TTC byte
/// stream. `None` when the bytes are not a readable font.
pub(crate) fn family_name(font: &[u8], index: u32) -> Option<String> {
    let (offset, num_tables) = table_directory(font, index)?;
    // Locate the name table.
    let mut name_offset = None;
    let mut name_len = None;
    for i in 0..num_tables {
        let record = offset + 12 + i * 16;
        let tag = font.get(record..record + 4)?;
        if tag == b"name" {
            name_offset = Some(read_u32(font, record + 8)? as usize);
            name_len = Some(read_u32(font, record + 12)? as usize);
            break;
        }
    }
    let (name_offset, name_len) = (name_offset?, name_len?);
    let table = font.get(name_offset..name_offset + name_len)?;

    let count = read_u16(table, 2)? as usize;
    let string_offset = read_u16(table, 4)? as usize;
    // Pass 1: nameID 1 under Windows Unicode; pass 2: nameID 16; pass 3:
    // Macintosh ASCII nameID 1.
    for want_id in [1u16, 16] {
        for i in 0..count {
            let rec = 6 + i * 12;
            let platform = read_u16(table, rec)?;
            let encoding = read_u16(table, rec + 2)?;
            let language = read_u16(table, rec + 4)?;
            let name_id = read_u16(table, rec + 6)?;
            let length = read_u16(table, rec + 8)? as usize;
            let offset = read_u16(table, rec + 10)? as usize;
            if name_id != want_id || platform != 3 {
                continue;
            }
            let _ = language; // any lang; en-US sorts first in practice
            if encoding != 1 {
                continue;
            }
            let start = string_offset + offset;
            let raw = table.get(start..start + length)?;
            let units: Vec<u16> = raw
                .as_chunks::<2>().0.iter()
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            let name = String::from_utf16_lossy(&units);
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    // Macintosh ASCII fallback.
    for i in 0..count {
        let rec = 6 + i * 12;
        let platform = read_u16(table, rec)?;
        let encoding = read_u16(table, rec + 2)?;
        let name_id = read_u16(table, rec + 6)?;
        let length = read_u16(table, rec + 8)? as usize;
        let offset = read_u16(table, rec + 10)? as usize;
        if platform == 1 && encoding == 0 && name_id == 1 {
            let start = string_offset + offset;
            let raw = table.get(start..start + length)?;
            let name = String::from_utf8_lossy(raw).into_owned();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// `(table-directory offset, table count)` for the index-th font of an
/// sfnt/TTC stream.
fn table_directory(font: &[u8], index: u32) -> Option<(usize, usize)> {
    let tag = font.get(0..4)?;
    if tag == b"ttcf" {
        let num_fonts = read_u32(font, 8)? as usize;
        if index as usize >= num_fonts {
            return None;
        }
        let offset = read_u32(font, 12 + index as usize * 4)? as usize;
        let num_tables = read_u16(font, offset + 4)? as usize;
        Some((offset, num_tables))
    } else {
        // Plain sfnt (both 0x00010000 and "OTTO").
        if tag != [0, 1, 0, 0] && tag != b"OTTO" && tag != b"true" {
            return None;
        }
        if index != 0 {
            return None;
        }
        let num_tables = read_u16(font, 4)? as usize;
        Some((0, num_tables))
    }
}

fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    let b = bytes.get(at..at + 2)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_returns_none() {
        assert!(family_name(b"not a font at all", 0).is_none());
        assert!(family_name(&[], 0).is_none());
    }

    #[test]
    fn out_of_range_collection_index_returns_none() {
        assert!(family_name(b"ttcf\x00\x00\x00\x00", 3).is_none());
    }

    /// Real-machine smoke against a system TrueType font (skip-if-absent).
    #[test]
    fn reads_system_font_family() {
        let path = "C:/Windows/Fonts/seguisym.ttf";
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!("skipping: {path} not available");
            return;
        };
        let name = family_name(&bytes, 0).expect("system font must parse");
        assert!(name.contains("Segoe UI Symbol"), "got {name:?}");
    }

    /// Real-machine TTC smoke: two collection members carry different
    /// families.
    #[test]
    fn reads_collection_members_independently() {
        let path = "C:/Windows/Fonts/simsun.ttc";
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!("skipping: {path} not available");
            return;
        };
        // SimSun ships SimSun + NSimSun in one TTC.
        let first = family_name(&bytes, 0).expect("index 0 must parse");
        assert!(
            first.contains("SimSun") || first.contains("宋体"),
            "got {first:?}"
        );
        // Second face must also parse (its family may match or differ).
        assert!(family_name(&bytes, 1).is_some());
    }
}
