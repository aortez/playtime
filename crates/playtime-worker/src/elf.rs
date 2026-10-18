use crate::{Error, Result};

fn range(bytes: &[u8], offset: u64, length: u64) -> Result<&[u8]> {
    let end = offset.checked_add(length).ok_or(Error::Capacity)?;
    if end > bytes.len() as u64 {
        return Err(Error::Invalid("truncated ELF"));
    }
    Ok(&bytes[offset as usize..end as usize])
}
fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
}
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// Inspect bytes, including static PIE. No target-name or filename inference.
pub fn verify_static_elf(bytes: &[u8]) -> Result<()> {
    let header = range(bytes, 0, 64)?;
    if &header[..7] != b"\x7fELF\x02\x01\x01" || u16_at(header, 18) != 62 {
        return Err(Error::UnsupportedPlatform);
    }
    if ![2, 3].contains(&u16_at(header, 16)) || u32_at(header, 20) != 1 || u16_at(header, 52) != 64
    {
        return Err(Error::Invalid("invalid ELF executable header"));
    }
    let start = u64_at(header, 32);
    let count = u16_at(header, 56) as u64;
    if u16_at(header, 54) != 56 || count == 0 || count > 1024 {
        return Err(Error::Invalid("invalid ELF program headers"));
    }
    let headers = range(bytes, start, count * 56)?;
    let mut load = false;
    for h in headers.chunks_exact(56) {
        let kind = u32_at(h, 0);
        let segment = range(bytes, u64_at(h, 8), u64_at(h, 32))?;
        if kind == 1 {
            load = true;
        }
        if kind == 3 {
            return Err(Error::DynamicExecutable);
        }
        if kind == 2 {
            if segment.len() % 16 != 0 {
                return Err(Error::Invalid("malformed dynamic segment"));
            }
            let mut terminated = false;
            for entry in segment.chunks_exact(16) {
                match u64_at(entry, 0) {
                    0 => {
                        terminated = true;
                        break;
                    }
                    1 => return Err(Error::DynamicExecutable),
                    _ => (),
                }
            }
            if !terminated {
                return Err(Error::Invalid("unterminated dynamic segment"));
            }
        }
    }
    if !load {
        return Err(Error::Invalid("ELF has no loadable segment"));
    }
    Ok(())
}
