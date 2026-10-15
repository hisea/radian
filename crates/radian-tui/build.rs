//! Builds the code-block highlighter's syntax set once, at compile time (ADR 0028 R2).
//!
//! `syntect`'s packed default set is loaded as-is, and the three vendored grammars under
//! `third_party/syntaxes` (TypeScript, TSX, TOML) are parsed and added to it. Parsing the
//! ~200 KB TypeScript YAML with `yaml-rust` costs ~2 s each, which is a visible freeze if it
//! happens on the first code block at runtime; doing it here and embedding the resulting
//! `.packdump` moves that cost to the build. The runtime only deserializes the dump.
//!
//! `into_builder` + `build` re-resolves context references across the whole set, which is why
//! the extras are added before `build` (a subset alone would leave dangling references).

use std::env;
use std::path::{Path, PathBuf};

use syntect::parsing::{SyntaxDefinition, SyntaxSet};

/// The vendored grammars, by `SyntaxDefinition::name`.
const EXTRA_SYNTAXES: &[(&str, &str)] = &[
    ("TypeScript", "TypeScript.sublime-syntax"),
    ("TypeScriptReact", "TypsecriptReact.sublime-syntax"),
    ("TOML", "TOML.sublime-syntax"),
];

fn main() {
    // Runtime, not `env!`: the build script's own `CARGO_MANIFEST_DIR` is baked when the
    // build script is compiled, so a workspace moved (renamed) after that kept reading the old
    // absolute path. Cargo sets this variable when it **runs** the build script.
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let root = Path::new(&manifest).join("../../third_party/syntaxes");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut builder = SyntaxSet::load_defaults_newlines().into_builder();
    for (name, file) in EXTRA_SYNTAXES {
        let path = root.join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        let definition = SyntaxDefinition::load_from_str(&source, true, Some(name))
            .unwrap_or_else(|error| panic!("cannot parse {} ({name}): {error}", path.display()));
        builder.add(definition);
    }
    let set = builder.build();
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("syntaxes.packdump");
    syntect::dumps::dump_to_uncompressed_file(&set, &out).expect("write the syntax dump");
}
