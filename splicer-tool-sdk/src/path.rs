//! String-level path helpers for builtin middleware. No actual
//! filesystem ops here; just rewrites of path-shaped strings.

/// Map any string to a filesystem-safe form by replacing every char
/// outside the portable allowlist `[A-Za-z0-9._-]` with `_`.
pub fn sanitize_for_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => c,
            _ => '_',
        })
        .collect()
}

/// Trim a leading `./` and any leading slashes so the result is safe
/// to feed segment-by-segment to a wasi:filesystem preopen.
pub fn strip_leading_slashes(raw: &str) -> &str {
    raw.trim_start_matches("./").trim_start_matches('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_safe_chars() {
        assert_eq!(sanitize_for_filename("abc.XYZ_0-9"), "abc.XYZ_0-9");
    }

    #[test]
    fn replaces_path_and_punctuation() {
        assert_eq!(
            sanitize_for_filename("my:svc/op@1::caller>callee"),
            "my_svc_op_1__caller_callee"
        );
    }

    #[test]
    fn is_idempotent() {
        let once = sanitize_for_filename("a/b c:d");
        assert_eq!(sanitize_for_filename(&once), once);
    }

    #[test]
    fn strip_leading_slashes_handles_common_cases() {
        assert_eq!(strip_leading_slashes("./recordings"), "recordings");
        assert_eq!(strip_leading_slashes("/abs/path"), "abs/path");
        assert_eq!(strip_leading_slashes("recordings"), "recordings");
        assert_eq!(strip_leading_slashes(""), "");
    }
}
