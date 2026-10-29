//! Just enough zip to pull one file out of a release archive (stored or
//! deflated, no zip64). The archive's sha256 is checked before this runs.

fn u16_at(b: &[u8], i: usize) -> Option<usize> {
    Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?) as usize)
}
fn u32_at(b: &[u8], i: usize) -> Option<usize> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?) as usize)
}

/// The first entry whose name `pick` accepts, decompressed.
pub fn extract(zip: &[u8], pick: impl Fn(&str) -> bool) -> Result<(String, Vec<u8>), String> {
    let bad = || "not a zip file (or a damaged one)".to_string();
    let floor = zip.len().saturating_sub(22 + 65535);
    let eocd = (floor..zip.len().saturating_sub(21))
        .rev()
        .find(|&i| zip[i..i + 4] == [0x50, 0x4b, 0x05, 0x06])
        .ok_or_else(bad)?;
    let entries = u16_at(zip, eocd + 10).ok_or_else(bad)?;
    let mut p = u32_at(zip, eocd + 16).ok_or_else(bad)?;
    for _ in 0..entries {
        if zip.get(p..p + 4) != Some(&[0x50, 0x4b, 0x01, 0x02][..]) {
            return Err(bad());
        }
        let method = u16_at(zip, p + 10).ok_or_else(bad)?;
        let csize = u32_at(zip, p + 20).ok_or_else(bad)?;
        let usize_ = u32_at(zip, p + 24).ok_or_else(bad)?;
        let nlen = u16_at(zip, p + 28).ok_or_else(bad)?;
        let elen = u16_at(zip, p + 30).ok_or_else(bad)?;
        let clen = u16_at(zip, p + 32).ok_or_else(bad)?;
        let local = u32_at(zip, p + 42).ok_or_else(bad)?;
        let name = String::from_utf8_lossy(zip.get(p + 46..p + 46 + nlen).ok_or_else(bad)?).to_string();
        p += 46 + nlen + elen + clen;
        if !pick(&name) {
            continue;
        }
        if zip.get(local..local + 4) != Some(&[0x50, 0x4b, 0x03, 0x04][..]) {
            return Err(bad());
        }
        let start = local + 30 + u16_at(zip, local + 26).ok_or_else(bad)? + u16_at(zip, local + 28).ok_or_else(bad)?;
        let data = zip.get(start..start + csize).ok_or_else(bad)?;
        let out = match method {
            0 => data.to_vec(),
            8 => miniz_oxide::inflate::decompress_to_vec_with_limit(data, usize_ + 1)
                .map_err(|e| format!("inflate {name}: {e:?}"))?,
            m => return Err(format!("{name}: unsupported zip method {m}")),
        };
        if out.len() != usize_ {
            return Err(format!("{name}: size mismatch"));
        }
        return Ok((name, out));
    }
    Err("file not found in the archive".into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn rejects_garbage() {
        assert!(super::extract(b"not a zip at all, just bytes", |_| true).is_err());
    }
}
