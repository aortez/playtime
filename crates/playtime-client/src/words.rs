//! Numbers and text as both clients word them: counts with thousands
//! separators, sizes, durations and excerpts. Pure functions with no I/O.

/// Formats a count with thousands separators, such as 300,000.
pub fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}
/// A count with its noun, such as 1 file or 40 files.
pub fn count(value: u64, one: &str, many: &str) -> String {
    format!(
        "{} {}",
        thousands(value),
        if value == 1 { one } else { many }
    )
}
/// Formats a byte count to one decimal, such as 512 bytes or 1.5 KiB.
pub fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} bytes"),
        1024..1_048_576 => format!("{:.1} KiB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MiB", bytes as f64 / 1_048_576.0),
    }
}
/// Formats a byte count up to GiB, exactly when it is a whole number of its
/// unit, such as 4 KiB or 1.5 MiB.
pub fn bytes(bytes: u64) -> String {
    for (unit, divisor) in [("GiB", 1u64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)] {
        if bytes >= divisor {
            return if bytes.is_multiple_of(divisor) {
                format!("{} {unit}", thousands(bytes / divisor))
            } else {
                format!("{:.1} {unit}", bytes as f64 / divisor as f64)
            };
        }
    }
    format!("{} {}", bytes, if bytes == 1 { "byte" } else { "bytes" })
}
/// Seconds as m:ss, or h:mm:ss from an hour.
pub fn clock(seconds: u64) -> String {
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}
/// A call's duration: milliseconds under a second, tenths under ten seconds
/// and whole seconds beyond.
pub fn duration_words(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 10_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{} s", thousands(ms / 1000))
    }
}
/// The first `bytes` of `text`, cut at a character boundary and marked when
/// anything follows.
pub fn excerpt(text: &str, bytes: usize) -> String {
    let mut end = bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut result = text[..end].to_owned();
    if end < text.len() {
        result.push_str(" …");
    }
    result
}
/// The text with its first character in upper case.
pub fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numbers_read_as_the_clients_word_them() {
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(count(1, "file", "files"), "1 file");
        assert_eq!(count(1200, "file", "files"), "1,200 files");
        assert_eq!(size(13_209_600), "12.6 MiB");
        assert_eq!(size(512), "512 bytes");
        assert_eq!(bytes(1 << 20), "1 MiB");
        assert_eq!(bytes(1), "1 byte");
        assert_eq!(bytes(3 << 29), "1.5 GiB");
        assert_eq!(clock(3601), "1:00:01");
        assert_eq!(clock(59), "0:59");
        assert_eq!(duration_words(999), "999 ms");
        assert_eq!(duration_words(5_640), "5.6 s");
        assert_eq!(duration_words(135_000), "135 s");
        assert_eq!(excerpt("héllo", 2), "h …");
        assert_eq!(excerpt("héllo", 6), "héllo");
        assert_eq!(capitalized("work 0:04"), "Work 0:04");
    }
}
