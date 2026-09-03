//! HPACK (RFC 7541). Full decoder (incl. Huffman), Chrome-style encoder.

use std::collections::HashMap;
use std::sync::OnceLock;

use super::tables::{HUFFMAN, STATIC_TABLE};
use crate::error::FetchError;

const DYNAMIC_MAX: usize = 65536; // Chrome HEADER_TABLE_SIZE

// ---------- integer coding (§5.1) ----------

fn encode_int(out: &mut Vec<u8>, mut value: u64, prefix_bits: u8, flags: u8) {
    let max_prefix = (1u64 << prefix_bits) - 1;
    if value < max_prefix {
        out.push(flags | value as u8);
        return;
    }
    out.push(flags | max_prefix as u8);
    value -= max_prefix;
    while value >= 128 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn decode_int(buf: &[u8], pos: &mut usize, prefix_bits: u8) -> Result<u64, FetchError> {
    if *pos >= buf.len() {
        return Err(FetchError::Http("hpack: truncated int".into()));
    }
    let max_prefix = (1u64 << prefix_bits) - 1;
    let mut value = (buf[*pos] as u64) & max_prefix;
    *pos += 1;
    if value < max_prefix {
        return Ok(value);
    }
    let mut shift = 0u32;
    loop {
        if *pos >= buf.len() {
            return Err(FetchError::Http("hpack: truncated int continuation".into()));
        }
        let b = buf[*pos];
        *pos += 1;
        value += ((b & 0x7f) as u64) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            return Ok(value);
        }
        if shift > 56 {
            return Err(FetchError::Http("hpack: int overflow".into()));
        }
    }
}

// ---------- huffman ----------

type HuffLookup = Vec<HashMap<u32, u16>>; // index = bit length

fn huff_lookup() -> &'static HuffLookup {
    static LOOKUP: OnceLock<HuffLookup> = OnceLock::new();
    LOOKUP.get_or_init(|| {
        let mut maps: HuffLookup = (0..=30).map(|_| HashMap::new()).collect();
        for (sym, &(code, bits)) in HUFFMAN.iter().enumerate() {
            if bits > 0 && bits <= 30 {
                maps[bits as usize].insert(code, sym as u16);
            }
        }
        maps
    })
}

pub fn huffman_decode(data: &[u8]) -> Result<Vec<u8>, FetchError> {
    let lookup = huff_lookup();
    let mut out = Vec::with_capacity(data.len() * 2);
    let mut code: u32 = 0;
    let mut len: u8 = 0;
    for &byte in data {
        for bit_idx in (0..8).rev() {
            let bit = (byte >> bit_idx) & 1;
            code = (code << 1) | bit as u32;
            len += 1;
            if len > 30 {
                return Err(FetchError::Http("hpack: huffman code too long".into()));
            }
            if let Some(&sym) = lookup[len as usize].get(&code) {
                if sym == 256 {
                    return Err(FetchError::Http("hpack: EOS in string".into()));
                }
                out.push(sym as u8);
                code = 0;
                len = 0;
            }
        }
    }
    // Remaining bits must be a prefix of EOS (all ones), at most 7 bits.
    if len >= 8 || (len > 0 && code != (1u32 << len) - 1) {
        return Err(FetchError::Http("hpack: bad huffman padding".into()));
    }
    Ok(out)
}

// ---------- strings ----------

/// Huffman-encode a byte string per RFC 7541 Appendix B.
/// Returns the encoded bytes (may be longer than input for short strings).
fn huffman_encode(input: &[u8]) -> Vec<u8> {
    let mut bit_buf: u64 = 0;
    let mut bit_count: u32 = 0;
    let mut out = Vec::with_capacity(input.len());
    for &byte in input {
        let (code, bits) = HUFFMAN[byte as usize];
        bit_buf = (bit_buf << bits) | code as u64;
        bit_count += bits as u32;
        while bit_count >= 8 {
            bit_count -= 8;
            out.push((bit_buf >> bit_count) as u8);
        }
    }
    // Pad remaining bits with EOS prefix (all 1-bits) to byte boundary.
    if bit_count > 0 {
        let pad = 8 - bit_count;
        out.push(((bit_buf << pad) | ((1u64 << pad) - 1)) as u8);
    }
    out
}

fn encode_string(out: &mut Vec<u8>, s: &[u8]) {
    // Chrome Huffman-encodes when shorter; raw otherwise.
    let huff = huffman_encode(s);
    if huff.len() < s.len() {
        encode_int(out, huff.len() as u64, 7, 0x80); // H=1
        out.extend_from_slice(&huff);
    } else {
        encode_int(out, s.len() as u64, 7, 0); // H=0
        out.extend_from_slice(s);
    }
}

fn decode_string(buf: &[u8], pos: &mut usize) -> Result<Vec<u8>, FetchError> {
    if *pos >= buf.len() {
        return Err(FetchError::Http("hpack: truncated string".into()));
    }
    let huff = buf[*pos] & 0x80 != 0;
    let len = decode_int(buf, pos, 7)? as usize;
    if *pos + len > buf.len() {
        return Err(FetchError::Http("hpack: truncated string data".into()));
    }
    let raw = &buf[*pos..*pos + len];
    *pos += len;
    if huff {
        huffman_decode(raw)
    } else {
        Ok(raw.to_vec())
    }
}

// ---------- dynamic table ----------

struct DynTable {
    entries: Vec<(Vec<u8>, Vec<u8>)>, // newest at end
    size: usize,
    max: usize,
}

impl DynTable {
    fn new(max: usize) -> Self {
        Self {
            entries: Vec::new(),
            size: 0,
            max,
        }
    }
    fn insert(&mut self, name: Vec<u8>, value: Vec<u8>) {
        let esz = name.len() + value.len() + 32;
        self.entries.push((name, value));
        self.size += esz;
        while self.size > self.max && !self.entries.is_empty() {
            let (n, v) = self.entries.remove(0);
            self.size -= n.len() + v.len() + 32;
        }
    }
    /// Absolute index: 1..=61 static, 62.. dynamic (newest first).
    fn get(&self, idx: usize) -> Option<(Vec<u8>, Vec<u8>)> {
        if idx >= 1 && idx <= STATIC_TABLE.len() {
            let (n, v) = STATIC_TABLE[idx - 1];
            return Some((n.as_bytes().to_vec(), v.as_bytes().to_vec()));
        }
        let Some(dyn_idx) = idx.checked_sub(STATIC_TABLE.len() + 1) else {
            return None; // hostile index 0 / protocol violation
        }; // 0 = newest
        if dyn_idx < self.entries.len() {
            return Some(self.entries[self.entries.len() - 1 - dyn_idx].clone());
        }
        None
    }
}

// ---------- encoder ----------

pub struct Encoder {
    dyn_table: DynTable,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    pub fn new() -> Self {
        Self {
            dyn_table: DynTable::new(DYNAMIC_MAX),
        }
    }

    /// Encode a header list in order. Indexed for exact static matches,
    /// literal-with-incremental-indexing otherwise (Chrome's strategy).
    /// Sensitive headers (cookie, authorization) use never-indexed to
    /// match Chrome's HPACK encoder : keeps the dynamic table identical.
    pub fn encode(&mut self, headers: &[(String, String)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, value) in headers {
            let name_l = name.to_ascii_lowercase();
            // Exact (name, value) static match → indexed.
            let exact = STATIC_TABLE
                .iter()
                .position(|(n, v)| *n == name_l && *v == value.as_str());
            if let Some(i) = exact {
                encode_int(&mut out, (i + 1) as u64, 7, 0x80);
                continue;
            }
            // Chrome marks sensitive headers as never-indexed to prevent
            // them from entering the dynamic table.
            let sensitive = matches!(
                name_l.as_str(),
                "cookie" | "authorization" | "proxy-authorization"
            );
            let name_idx = STATIC_TABLE.iter().position(|(n, _)| *n == name_l);
            if sensitive {
                // Never indexed (0x10 prefix, 4-bit integer).
                match name_idx {
                    Some(i) => encode_int(&mut out, (i + 1) as u64, 4, 0x10),
                    None => {
                        out.push(0x10);
                        encode_string(&mut out, name_l.as_bytes());
                    }
                }
                encode_string(&mut out, value.as_bytes());
                // Do NOT insert into dynamic table.
            } else {
                // Literal with incremental indexing (0x40 prefix, 6-bit integer).
                match name_idx {
                    Some(i) => encode_int(&mut out, (i + 1) as u64, 6, 0x40),
                    None => {
                        out.push(0x40);
                        encode_string(&mut out, name_l.as_bytes());
                    }
                }
                encode_string(&mut out, value.as_bytes());
                self.dyn_table
                    .insert(name_l.into_bytes(), value.clone().into_bytes());
            }
        }
        out
    }
}

// ---------- decoder ----------

pub struct Decoder {
    dyn_table: DynTable,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            dyn_table: DynTable::new(4096),
        } // server-controlled via SETTINGS
    }

    pub fn decode(&mut self, block: &[u8]) -> Result<Vec<(String, String)>, FetchError> {
        let mut headers = Vec::new();
        let mut pos = 0usize;
        while pos < block.len() {
            let b = block[pos];
            if b & 0x80 != 0 {
                // Indexed.
                let idx = decode_int(block, &mut pos, 7)? as usize;
                let (n, v) = self
                    .dyn_table
                    .get(idx)
                    .ok_or_else(|| FetchError::Http(format!("hpack: bad index {idx}")))?;
                headers.push((
                    String::from_utf8_lossy(&n).into(),
                    String::from_utf8_lossy(&v).into(),
                ));
            } else if b & 0xc0 == 0x40 {
                // Literal, incremental indexing.
                let (name, value) = self.decode_literal(block, &mut pos, 6)?;
                self.dyn_table
                    .insert(name.clone().into_bytes(), value.clone().into_bytes());
                headers.push((name, value));
            } else if b & 0xe0 == 0x20 {
                // Dynamic table size update.
                let new_max = decode_int(block, &mut pos, 5)? as usize;
                // RFC 7541 §4.2: must not exceed what we advertised
                // (Chrome's HEADER_TABLE_SIZE = 65536). An uncapped
                // update lets a hostile server balloon our decoder
                // table without bound.
                if new_max > DYNAMIC_MAX {
                    return Err(FetchError::Http(format!(
                        "hpack: table size update {new_max} exceeds {DYNAMIC_MAX}"
                    )));
                }
                self.dyn_table.max = new_max;
                while self.dyn_table.size > self.dyn_table.max && !self.dyn_table.entries.is_empty()
                {
                    let (n, v) = self.dyn_table.entries.remove(0);
                    self.dyn_table.size -= n.len() + v.len() + 32;
                }
            } else {
                // Literal without indexing (0x00) / never indexed (0x10).
                let (name, value) = self.decode_literal(block, &mut pos, 4)?;
                headers.push((name, value));
            }
        }
        Ok(headers)
    }

    fn decode_literal(
        &mut self,
        block: &[u8],
        pos: &mut usize,
        prefix: u8,
    ) -> Result<(String, String), FetchError> {
        let name_idx = decode_int(block, pos, prefix)? as usize;
        let name = if name_idx == 0 {
            decode_string(block, pos)?
        } else {
            self.dyn_table
                .get(name_idx)
                .ok_or_else(|| FetchError::Http(format!("hpack: bad name index {name_idx}")))?
                .0
        };
        let value = decode_string(block, pos)?;
        Ok((
            String::from_utf8_lossy(&name).into(),
            String::from_utf8_lossy(&value).into(),
        ))
    }
}
