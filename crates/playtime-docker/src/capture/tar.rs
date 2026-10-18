use super::*;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, io::Read};

struct Stream<'a, R> {
    reader: R,
    raw: Vec<u8>,
    hash: Sha256,
    limit: u64,
    cancel: &'a CancellationToken,
}
impl<R: Read> Stream<'_, R> {
    fn read(&mut self, into: &mut [u8]) -> Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(Error::new(
                Failure::Cancelled,
                "capture",
                "validation cancelled",
            ));
        }
        let n = self
            .reader
            .read(into)
            .map_err(|_| malformed("tar read failed"))?;
        if (self.raw.len() as u64)
            .checked_add(n as u64)
            .ok_or_else(capacity)?
            > self.limit
        {
            return Err(capacity());
        }
        self.raw.extend_from_slice(&into[..n]);
        self.hash.update(&into[..n]);
        Ok(n)
    }
    fn exact(&mut self, mut into: &mut [u8]) -> Result<()> {
        while !into.is_empty() {
            let n = self.read(into)?;
            if n == 0 {
                return Err(malformed("truncated tar"));
            }
            into = &mut into[n..];
        }
        Ok(())
    }
    fn block(&mut self) -> Result<[u8; 512]> {
        let mut b = [0; 512];
        self.exact(&mut b)?;
        Ok(b)
    }
    fn body(&mut self, size: u64) -> Result<(Vec<u8>, String)> {
        let size: usize = size.try_into().map_err(|_| capacity())?;
        let mut data = Vec::new();
        let mut hash = Sha256::new();
        let mut b = [0; 8192];
        while data.len() < size {
            let n = (size - data.len()).min(b.len());
            self.exact(&mut b[..n])?;
            hash.update(&b[..n]);
            data.extend_from_slice(&b[..n]);
        }
        let pad = (512 - size % 512) % 512;
        self.exact(&mut b[..pad])?;
        if b[..pad].iter().any(|b| *b != 0) {
            return Err(malformed("nonzero member padding"));
        }
        Ok((data, hex_digest(hash.finalize())))
    }
}
fn string(bytes: &[u8]) -> Result<&str> {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    if bytes[end..].iter().any(|b| *b != 0) {
        return Err(malformed("noncanonical header string"));
    }
    std::str::from_utf8(&bytes[..end]).map_err(|_| malformed("non UTF-8 header"))
}
fn number(bytes: &[u8]) -> Result<u64> {
    let text = std::str::from_utf8(bytes).map_err(|_| malformed("unsupported numeric encoding"))?;
    let text = text.trim_matches(['\0', ' ']);
    if text.is_empty() {
        return Ok(0);
    }
    text.bytes().try_fold(0u64, |n, b| {
        if !(b'0'..=b'7').contains(&b) {
            return Err(malformed("invalid octal field"));
        }
        n.checked_mul(8)
            .and_then(|n| n.checked_add((b - b'0') as u64))
            .ok_or_else(capacity)
    })
}
fn header(b: &[u8; 512]) -> Result<()> {
    let checksum: u64 = b
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if (148..156).contains(&i) {
                32
            } else {
                *b as u64
            }
        })
        .sum();
    if checksum != number(&b[148..156])?
        || &b[257..265] != b"ustar\x0000"
        || b[500..].iter().any(|b| *b != 0)
        || number(&b[329..337])? != 0
        || number(&b[337..345])? != 0
    {
        return Err(malformed("invalid tar header/checksum/format"));
    }
    Ok(())
}
fn path(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 4096
        || value.contains('\0')
        || value.starts_with('/')
        || value.split('/').count() > 32
        || value.split('/').any(|p| matches!(p, "" | "." | ".."))
    {
        Err(malformed("invalid logical path"))
    } else {
        Ok(())
    }
}
fn pax(bytes: &[u8]) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let suffix = &bytes[offset..];
        let space = suffix
            .iter()
            .position(|b| *b == b' ')
            .ok_or_else(|| malformed("missing PAX length"))?;
        if space == 0 || space > 5 || suffix[0] == b'0' {
            return Err(malformed("invalid PAX length"));
        }
        let mut length = 0usize;
        for digit in &suffix[..space] {
            if !digit.is_ascii_digit() {
                return Err(malformed("invalid PAX length"));
            }
            length = length
                .checked_mul(10)
                .and_then(|n| n.checked_add((digit - b'0') as usize))
                .ok_or_else(capacity)?;
        }
        if length > suffix.len() || length <= space + 2 || suffix[length - 1] != b'\n' {
            return Err(malformed("invalid PAX record size"));
        }
        let record = std::str::from_utf8(&suffix[space + 1..length - 1])
            .map_err(|_| malformed("invalid PAX UTF-8"))?;
        let (key, value) = record
            .split_once('=')
            .ok_or_else(|| malformed("invalid PAX record"))?;
        if !matches!(key, "path" | "linkpath") || values.insert(key.into(), value.into()).is_some()
        {
            return Err(malformed("unsupported or duplicate PAX field"));
        }
        offset = offset.checked_add(length).ok_or_else(capacity)?;
    }
    if values.is_empty() {
        return Err(malformed("empty PAX extension"));
    }
    Ok(values)
}
pub(super) fn validate(
    reader: impl Read,
    profile: &CaptureProfile,
    cancel: &CancellationToken,
) -> Result<ValidatedWorkspace> {
    let limits = &profile.limits;
    let mut stream = Stream {
        reader,
        raw: Vec::new(),
        hash: Sha256::new(),
        limit: limits.tar_bytes,
        cancel,
    };
    let mut entries: Vec<CapturedEntry> = Vec::new();
    let mut seen: BTreeMap<String, u8> = BTreeMap::new();
    let mut bodies = Vec::new();
    let mut retained_bytes = 0u64;
    let mut extension = None;
    let mut index_size = 2u64;
    loop {
        let b = stream.block()?;
        if b == [0; 512] {
            if extension.is_some() || stream.block()? != [0; 512] {
                return Err(malformed("missing end blocks or dangling extension"));
            }
            let mut padding = 0usize;
            loop {
                let mut block = [0; 512];
                let n = stream.read(&mut block)?;
                if n == 0 {
                    break;
                }
                padding = padding.checked_add(n).ok_or_else(capacity)?;
                if padding > 10240 || block[..n].iter().any(|b| *b != 0) {
                    return Err(malformed("nonzero or excessive tar tail"));
                }
            }
            if !stream.raw.len().is_multiple_of(512) {
                return Err(malformed("unaligned tar tail"));
            }
            break;
        }
        header(&b)?;
        let size = number(&b[124..136])?;
        let kind = b[156];
        if kind == b'x' {
            if extension.is_some() || size > 16384 {
                return Err(malformed("duplicate or oversized PAX extension"));
            }
            extension = Some(pax(&stream.body(size)?.0)?);
            continue;
        }
        if !matches!(kind, b'0' | b'1' | b'2' | b'5') {
            return Err(malformed("unsupported tar entry type"));
        }
        if entries.len() as u64 >= limits.entries {
            return Err(capacity());
        }
        let mode = number(&b[100..108])?;
        if mode > 0o7777
            || number(&b[108..116])? != 1000
            || number(&b[116..124])? != 1000
            || number(&b[136..148])? != 0
            || !string(&b[265..297])?.is_empty()
            || !string(&b[297..329])?.is_empty()
        {
            return Err(malformed("non-normalized tar metadata"));
        }
        let prefix = string(&b[345..500])?;
        let base = string(&b[..100])?;
        let mut name = if prefix.is_empty() {
            base.into()
        } else {
            format!("{prefix}/{base}")
        };
        let mut link = string(&b[157..257])?.to_owned();
        if let Some(pax) = extension.take() {
            if let Some(p) = pax.get("path") {
                name = p.clone();
            }
            if let Some(p) = pax.get("linkpath") {
                if !matches!(kind, b'1' | b'2') {
                    return Err(malformed("linkpath on nonlink"));
                }
                link = p.clone();
            }
        }
        if kind == b'5' && name.ends_with('/') {
            name.pop();
        }
        path(&name)?;
        if entries
            .last()
            .is_some_and(|e| e.path.as_bytes() >= name.as_bytes())
            || seen.contains_key(&name)
        {
            return Err(malformed("duplicate or unordered logical path"));
        }
        let mut ancestor = name.as_str();
        while let Some((parent, _)) = ancestor.rsplit_once('/') {
            if seen.get(parent) != Some(&b'5') {
                return Err(malformed("missing or non-directory ancestor"));
            }
            ancestor = parent;
        }
        if kind != b'0' && size != 0 {
            return Err(malformed("nonregular payload"));
        }
        let (sha256, content_slot, link) = match kind {
            b'0' => {
                if !link.is_empty() {
                    return Err(malformed("regular member has link"));
                }
                retained_bytes = retained_bytes.checked_add(size).ok_or_else(capacity)?;
                if size > limits.file_bytes || retained_bytes > limits.retained_bytes {
                    return Err(capacity());
                }
                profile.capacity(
                    (stream.raw.len() as u64)
                        .checked_add(size)
                        .and_then(|n| n.checked_add(retained_bytes))
                        .ok_or_else(capacity)?,
                    bodies.len() as u64 + 4,
                )?;
                let (body, hash) = stream.body(size)?;
                let slot = bodies.len();
                bodies.push(body);
                (Some(hash), Some(slot), None)
            }
            b'5' => {
                if !link.is_empty() {
                    return Err(malformed("directory has link"));
                }
                (None, None, None)
            }
            _ => {
                if link.is_empty() || link.len() > 4096 || link.contains('\0') {
                    return Err(malformed("invalid link target"));
                }
                if kind == b'1' {
                    path(&link)?;
                    if seen.get(&link) != Some(&b'0') {
                        return Err(malformed(
                            "hardlink must reference an earlier regular member",
                        ));
                    }
                }
                (None, None, Some(link))
            }
        };
        let entry = CapturedEntry {
            path: name.clone(),
            entry_type: (kind as char).to_string(),
            mode: mode as u32,
            size_bytes: size,
            sha256,
            link,
            content_slot,
        };
        let encoded = serde_json::to_vec(&entry).map_err(|_| malformed("index encoding"))?;
        index_size = index_size
            .checked_add(encoded.len() as u64)
            .and_then(|n| n.checked_add(u64::from(!entries.is_empty())))
            .ok_or_else(capacity)?;
        if index_size > limits.index_bytes {
            return Err(capacity());
        }
        seen.insert(name, kind);
        entries.push(entry);
    }
    let index = serde_json::to_vec(&entries).map_err(|_| malformed("index encoding"))?;
    profile.capacity(
        (stream.raw.len() as u64)
            .checked_add(retained_bytes)
            .and_then(|n| n.checked_add(index.len() as u64))
            .ok_or_else(capacity)?,
        bodies.len() as u64 + 3,
    )?;
    Ok(ValidatedWorkspace {
        tar: stream.raw,
        sha256: hex_digest(stream.hash.finalize()),
        entries,
        bodies,
        index,
        retained_bytes,
    })
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
