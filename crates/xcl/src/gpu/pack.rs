//! Extracting shader blobs from packed archives without a table of contents.
//!
//! RPF9 packs ("9FPR" magic) encrypt their TOC, but the shader packs inside store every
//! shader as a plain record (found empirically, 23 869 records in a 411 MB pack):
//!
//! ```text
//! u8 len | name[len-1] (identifier) | 0x00 | u8 flag | u32 csize | u32 unk | u32 csize | u32 usize | deflate-raw[csize]
//! ```
//!
//! The record payload inflates to a DXBC container. Names longer than 255 bytes do not
//! fit the length byte, so a few names come out truncated.

pub const RPF9_MAGIC: &[u8; 4] = b"9FPR";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub name: String,
    /// offset of the record (its length byte)
    pub offset: usize,
    pub flag: u8,
    /// offset and size of the raw deflate stream
    pub data: usize,
    pub csize: usize,
    pub usize: usize,
}

fn ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Find shader records in `d`, starting at `from`.
pub fn scan(d: &[u8], from: usize) -> Vec<Record> {
    let mut out = Vec::new();
    let mut i = from;
    while i < d.len().saturating_sub(32) {
        let len = d[i] as usize;
        let Some(p) = i.checked_add(len) else { break };
        let Some(header_end) = p.checked_add(18) else { break };
        if len >= 4
            && header_end <= d.len()
            && d[p] == 0
            && (d[i + 1].is_ascii_alphabetic() || d[i + 1] == b'_')
            && d[i + 2..p].iter().all(|&c| ident(c))
        {
            let (c1, c2, u) = (rd32(d, p + 2) as usize, rd32(d, p + 10) as usize, rd32(d, p + 14) as usize);
            let Some(data_end) = header_end.checked_add(c1) else {
                i += 1;
                continue;
            };
            if c1 == c2
                && c1 > 0
                && c1 < 16 << 20
                && u > 0
                && u < 64 << 20
                && data_end <= d.len()
            {
                out.push(Record {
                    name: String::from_utf8_lossy(&d[i + 1..p]).into_owned(),
                    offset: i,
                    flag: d[p + 1],
                    data: header_end,
                    csize: c1,
                    usize: u,
                });
                i = data_end;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Inflate a record. `None` when the stream is not valid deflate.
pub fn inflate(d: &[u8], r: &Record) -> Option<Vec<u8>> {
    let end = r.data.checked_add(r.csize)?;
    let compressed = d.get(r.data..end)?;
    let unpacked = miniz_oxide::inflate::decompress_to_vec_with_limit(compressed, r.usize).ok()?;
    (unpacked.len() == r.usize).then_some(unpacked)
}

/// Stage prefix of a shader name (`PS_Foo` → `PS`).
pub fn name_stage(name: &str) -> Option<&'static str> {
    let up = name.get(..2)?.to_ascii_uppercase();
    ["PS", "VS", "MS", "GS", "CS", "HS", "DS", "AS"].into_iter().find(|s| *s == up)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, payload: &[u8]) -> Vec<u8> {
        let z = miniz_oxide::deflate::compress_to_vec(payload, 6);
        let mut v = vec![(name.len() + 1) as u8];
        v.extend_from_slice(name.as_bytes());
        v.push(0);
        v.push(50);
        for x in [z.len() as u32, 0xddce7, z.len() as u32, payload.len() as u32] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v.extend_from_slice(&z);
        v
    }

    #[test]
    fn finds_records_between_noise() {
        let mut pack = b"9FPR".to_vec();
        pack.resize(0x40, 0xAA);
        let a = crate::gpu::xbox::tests::sample(0, &[crate::gpu::xbox::tests::fssm(&crate::gpu::xbox::tests::CODE, &[(5, 1)])]);
        pack.extend(record("PS_Test_1234abcd_Wrapped", &a));
        pack.extend_from_slice(&[0x13, 0x37, 0, 1, 2, 3]);
        pack.extend(record("VS_Other", b"DXBC-not-really"));
        pack.extend_from_slice(&[0u8; 40]);
        let recs = scan(&pack, 0x40);
        assert_eq!(recs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), vec!["PS_Test_1234abcd_Wrapped", "VS_Other"]);
        assert_eq!(inflate(&pack, &recs[0]).unwrap(), a);
        assert_eq!(recs[0].flag, 50);
        assert_eq!(name_stage(&recs[0].name), Some("PS"));
        assert_eq!(name_stage("ptfx_x"), None);
    }

    #[test]
    fn inflate_rejects_hostile_ranges_and_size_mismatches() {
        let payload = b"synthetic payload";
        let compressed = miniz_oxide::deflate::compress_to_vec(payload, 6);
        let base = Record {
            name: "PS_Synthetic".into(),
            offset: 0,
            flag: 0,
            data: 0,
            csize: compressed.len(),
            usize: payload.len(),
        };
        assert_eq!(inflate(&compressed, &base).as_deref(), Some(payload.as_slice()));

        let mut record = base.clone();
        record.data = usize::MAX;
        record.csize = 2;
        assert_eq!(inflate(&compressed, &record), None);

        record = base.clone();
        record.data = compressed.len() + 1;
        assert_eq!(inflate(&compressed, &record), None);

        record = base;
        record.usize += 1;
        assert_eq!(inflate(&compressed, &record), None);
    }

    #[test]
    fn scan_is_bounded_for_hostile_offsets_and_arbitrary_bytes() {
        assert!(scan(&[0; 64], usize::MAX).is_empty());

        let mut state = 0x6f6c_796d_7075_7355u64;
        for len in 0..512usize {
            let mut bytes = vec![0u8; len];
            for byte in &mut bytes {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = state as u8;
            }
            for from in [0, len / 2, len, len.saturating_add(1), usize::MAX] {
                let records = scan(&bytes, from);
                let mut previous_end = from;
                for record in records {
                    let end = record.data.checked_add(record.csize).expect("record range");
                    assert!(record.offset >= previous_end);
                    assert!(end <= bytes.len());
                    assert!(record.csize < 16 << 20);
                    assert!(record.usize < 64 << 20);
                    previous_end = end;
                }
            }
        }

        let payload: Vec<u8> = (0..128).map(|value| value as u8).collect();
        let complete = record("PS_TruncationProbe", &payload);
        for end in 0..=complete.len() {
            for record in scan(&complete[..end], 0) {
                assert!(record.data.checked_add(record.csize).is_some_and(|record_end| record_end <= end));
            }
        }
        assert_eq!(scan(&complete, 0).len(), 1);
    }
}
