use sha2::{Digest, Sha256};

/// Longest model-facing tool name accepted by the providers we target.
pub const MAX_ALIAS_LEN: usize = 64;

const SUFFIX_HEX_LEN: usize = 8;

/// The model-facing name for an MCP tool.
///
/// It depends only on `server_id` and the tool's own name, so adding, removing or reordering
/// other tools never changes it, and tool calls in old transcripts stay resolvable.
///
/// The form is `server__tool` with every character outside `[A-Za-z0-9_-]` replaced by `_`.
/// Only when that replacement changed something, or the result is too long, the readable part
/// is cut to fit and a fixed 8-hex-digit hash of the original names is appended, so distinct
/// originals that sanitize alike still differ.
pub fn tool_alias(server_id: &str, tool_name: &str) -> String {
    let readable = format!("{}__{}", sanitize(server_id), sanitize(tool_name));
    let changed = readable != format!("{server_id}__{tool_name}");
    if !changed && readable.len() <= MAX_ALIAS_LEN {
        return readable;
    }
    let mut hasher = Sha256::new();
    hasher.update(server_id.as_bytes());
    hasher.update([0]);
    hasher.update(tool_name.as_bytes());
    let digest = hasher.finalize();
    let suffix: String = digest
        .iter()
        .take(SUFFIX_HEX_LEN / 2)
        .map(|b| format!("{b:02x}"))
        .collect();
    // `readable` is ASCII after sanitizing, so byte truncation is safe.
    let keep = MAX_ALIAS_LEN - SUFFIX_HEX_LEN - 1;
    let head = &readable[..readable.len().min(keep)];
    format!("{head}_{suffix}")
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_stay_readable() {
        assert_eq!(tool_alias("fs", "read_file"), "fs__read_file");
    }

    #[test]
    fn invalid_characters_get_a_fixed_suffix() {
        let alias = tool_alias("fs", "Bad Name!");
        assert!(alias.starts_with("fs__Bad_Name__"), "{alias}");
        assert_eq!(alias.len(), "fs__Bad_Name_".len() + 1 + SUFFIX_HEX_LEN);
        // Distinct originals that sanitize alike stay distinct.
        assert_ne!(alias, tool_alias("fs", "Bad Name?"));
    }

    #[test]
    fn long_names_are_cut_to_the_limit() {
        let alias = tool_alias("server", &"x".repeat(200));
        assert_eq!(alias.len(), MAX_ALIAS_LEN);
        assert!(
            alias
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
    }

    #[test]
    fn alias_is_deterministic() {
        assert_eq!(tool_alias("a b", "c.d"), tool_alias("a b", "c.d"));
    }
}
