//! Small helpers shared across the crate.

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub(crate) fn default_true() -> bool {
    true
}

pub(crate) fn is_true(v: &bool) -> bool {
    *v
}

pub(crate) fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

/// Turns a display name into a file-system friendly slug (`Get users` ->
/// `get-users`). Never returns an empty string.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut dash = false;
    for ch in name.chars() {
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.len() >= 60 {
            break;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("untitled");
    }
    out
}

/// Formats a byte count for humans (`1.5 KB`).
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[unit]) }
}

/// Formats a duration in milliseconds for humans (`812 ms`, `1.24 s`).
pub fn human_ms(ms: f64) -> String {
    if ms < 1000.0 { format!("{} ms", ms.round() as u64) } else { format!("{:.2} s", ms / 1000.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slugify("Get users"), "get-users");
        assert_eq!(slugify("  /api/v1/{id}  "), "api-v1-id");
        assert_eq!(slugify("***"), "untitled");
        assert_eq!(slugify("Ambil Data Pengguna"), "ambil-data-pengguna");
    }

    #[test]
    fn bytes() {
        assert_eq!(human_bytes(10), "10 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
    }
}
