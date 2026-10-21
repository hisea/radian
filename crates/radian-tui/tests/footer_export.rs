//! `radian footer export <preset>` prints the preset's TOML exactly (ADR 0027),
//! so a user can copy it into their config under `[tui.footer]` and edit it.
//! Like `tests/version.rs`, this runs the binary as an ordinary command: no TUI, no session.

use std::process::Command;

fn shipped(preset: &str) -> &'static str {
    match preset {
        "deck" => include_str!("../presets/deck.toml"),
        "full" => include_str!("../presets/full.toml"),
        "pi" => include_str!("../presets/pi.toml"),
        "classic" => include_str!("../presets/classic.toml"),
        _ => panic!("no shipped preset {preset}"),
    }
}

/// Every preset prints its TOML file byte for byte on stdout, with nothing on stderr.
#[test]
fn footer_export_prints_the_preset_toml_exactly() {
    for preset in ["deck", "full", "pi", "classic"] {
        let output = Command::new(env!("CARGO_BIN_EXE_radian"))
            .args(["footer", "export", preset])
            .output()
            .expect("run radian footer export");
        assert_eq!(output.status.code(), Some(0), "{preset}: exit code");
        assert_eq!(
            output.stdout,
            shipped(preset).as_bytes(),
            "{preset}: stdout is the preset file exactly"
        );
        assert!(
            output.stderr.is_empty(),
            "{preset}: stderr {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// An unknown preset is a usage error (exit 2) naming the four presets on stderr.
#[test]
fn footer_export_names_the_four_for_an_unknown_preset() {
    let output = Command::new(env!("CARGO_BIN_EXE_radian"))
        .args(["footer", "export", "bogus"])
        .output()
        .expect("run radian footer export");
    assert_eq!(output.status.code(), Some(2), "exit code");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    for preset in ["deck", "full", "pi", "classic"] {
        assert!(stderr.contains(preset), "stderr names {preset}: {stderr:?}");
    }
    assert!(output.stdout.is_empty(), "nothing on stdout");
}
