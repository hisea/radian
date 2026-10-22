//! Writing a copy to the system clipboard (as an [`crate::app::Effect`], never inside
//! `update` or a frame).
//!
//! The default transport is **OSC 52**: `\x1b]52;c;<base64>\x07` on stdout. It needs no
//! child process and works over ssh, but it is fire-and-forget: the terminal never answers,
//! so Radian cannot know whether the paste really happened. The status bar therefore says
//! `copied N chars (via OSC 52)` — what Radian did, not what the clipboard holds.
//! `tui.clipboard = "pbcopy"` runs macOS's `pbcopy` and *can* report failure honestly;
//! `"off"` disables copying (a selection still draws).

/// How large a selection may get before copying is refused. One mebibyte of text is far past
/// anything a transcript selection should be; refusing bounds both the OSC 52 write and the
/// `pbcopy` pipe.
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;

/// The `tui.clipboard` transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClipboardMode {
    /// OSC 52 on stdout (the default).
    #[default]
    Osc52,
    /// macOS's `pbcopy`, spawned by the event loop. Configured elsewhere it falls back to
    /// [`ClipboardMode::Osc52`] with a startup warning.
    PbCopy,
    /// No copying: the selection draws, releasing it does nothing.
    Off,
}

impl ClipboardMode {
    /// The config spelling. `None` for anything else, so the caller warns and falls back to
    /// the default (the `tui.*` rule for a display preference).
    pub fn parse(name: &str) -> Option<Self> {
        if !radian_host::config::CLIPBOARD_MODES.contains(&name) {
            return None;
        }
        match name {
            "osc52" => Some(Self::Osc52),
            "pbcopy" => Some(Self::PbCopy),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    /// The config spelling of this mode.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Osc52 => radian_host::config::CLIPBOARD_MODES[0],
            Self::PbCopy => radian_host::config::CLIPBOARD_MODES[1],
            Self::Off => radian_host::config::CLIPBOARD_MODES[2],
        }
    }

    /// Whether this mode can run on this platform. `pbcopy` is a macOS program.
    pub fn supported_on_this_platform(self) -> bool {
        match self {
            Self::PbCopy => cfg!(target_os = "macos"),
            Self::Osc52 | Self::Off => true,
        }
    }
}

/// The OSC 52 sequence for `text`: `\x1b]52;c;<base64>\x07`, the clipboard (`c`) selection.
/// RFC 4648 base64, padded, ASCII only — every byte is one char, so the encoded length is
/// predictable (`4 * ceil(n / 3)`).
pub fn osc52(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(10 + bytes.len().div_ceil(3) * 4);
    out.push_str("\x1b]52;c;");
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18 & 63) as usize] as char);
        out.push(TABLE[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out.push('\x07');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_round_trip_and_unknown_names_are_none() {
        for mode in [
            ClipboardMode::Osc52,
            ClipboardMode::PbCopy,
            ClipboardMode::Off,
        ] {
            assert_eq!(
                ClipboardMode::parse(match mode {
                    ClipboardMode::Osc52 => "osc52",
                    ClipboardMode::PbCopy => "pbcopy",
                    ClipboardMode::Off => "off",
                }),
                Some(mode)
            );
        }
        assert_eq!(ClipboardMode::parse("system"), None);
        assert_eq!(ClipboardMode::parse(""), None);
        assert_eq!(ClipboardMode::default(), ClipboardMode::Osc52);
    }

    #[test]
    fn osc52_encodes_base64_with_the_clipboard_selection() {
        // RFC 4648 test vectors.
        let plain = osc52("foobar");
        assert_eq!(plain, "\x1b]52;c;Zm9vYmFy\x07");
        assert_eq!(osc52("foob"), "\x1b]52;c;Zm9vYg==\x07");
        assert_eq!(osc52("foobai"), "\x1b]52;c;Zm9vYmFp\x07");
        assert_eq!(osc52(""), "\x1b]52;c;\x07");
        // The last two table characters (`+`/`/`, indices 62/63) have no other vector: `~`
        // ends in six ones (index 62), `?` is six ones (index 63). Without these, swapping
        // the table to its URL-safe variant (`-_`) — or any transposition of the tail —
        // stays green.
        assert_eq!(osc52("~~~"), "\x1b]52;c;fn5+\x07");
        assert_eq!(osc52("???"), "\x1b]52;c;Pz8/\x07");
        // Multi-byte UTF-8 goes through as bytes and survives a round trip by hand.
        let seq = osc52("日");
        assert_eq!(seq, "\x1b]52;c;5pel\x07");
        // 5pel decodes to E6 97 A5 = U+65E5.
        assert_eq!(
            seq.trim_start_matches("\x1b]52;c;")
                .trim_end_matches('\x07'),
            "5pel"
        );
    }

    #[test]
    fn the_size_cap_is_one_mebibyte_and_pbcopy_is_platform_checked() {
        assert_eq!(MAX_CLIPBOARD_BYTES, 1024 * 1024);
        // `supported_on_this_platform` agrees with the compile target, so a wrong-platform
        // config is always caught at startup, never at copy time.
        assert_eq!(
            ClipboardMode::PbCopy.supported_on_this_platform(),
            cfg!(target_os = "macos")
        );
        assert!(ClipboardMode::Osc52.supported_on_this_platform());
        assert!(ClipboardMode::Off.supported_on_this_platform());
    }
}
