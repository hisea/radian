//! The code-block highlighter's syntax set (ADR 0028 R2): check the committed dump, hand it to
//! the compiler.
//!
//! The set is syntect's packed default set with the three vendored grammars under
//! `third_party/syntaxes` (TypeScript, TSX, TOML) merged in. Generating it means parsing the
//! ~200 KB TypeScript YAML, and `syntect`'s YAML loader **compiles every pattern's regex** to
//! validate it and then throws the compiled regex away (the dump stores pattern strings; the
//! regexes are compiled lazily at runtime). That cost 11.7 s of CPU for the two TypeScript
//! grammars alone, the slowest single unit of a cold `cargo test --workspace --no-run` — and
//! every fresh worktree paid it.
//!
//! So the merged set is generated once and committed as `assets/syntaxes.packdump`; what is left
//! to do on every build is to notice when the grammars it was generated from changed:
//!
//! - `assets/syntaxes.packdump.digest` holds the digest of the three grammar files plus the
//!   recipe that produced the dump. A mismatch fails the build, so a stale dump cannot ship
//!   silently.
//! - The dump is deserialized here too, with the same syntect the runtime uses: a syntect whose
//!   dump format changed fails the build rather than the first code block at runtime.
//! - `RADIAN_REGENERATE_SYNTAXES=1 cargo build -p radian-tui` regenerates both files from the
//!   vendored grammars; commit them. That is the only slow path left.
//!
//! The dump is byte-for-byte reproducible: an unoptimized and an `opt-level = 3` build of this
//! script produce the same bytes, so the digest is a property of the inputs alone.

use std::env;
use std::path::{Path, PathBuf};

use syntect::parsing::{SyntaxDefinition, SyntaxSet};

/// The vendored grammars, by `SyntaxDefinition::name`.
const EXTRA_SYNTAXES: &[(&str, &str)] = &[
    ("TypeScript", "TypeScript.sublime-syntax"),
    ("TypeScriptReact", "TypsecriptReact.sublime-syntax"),
    ("TOML", "TOML.sublime-syntax"),
];

/// The committed dump, the digest of the inputs it was generated from, and the file the binary
/// embeds (`src/highlight.rs` includes it from `$OUT_DIR`).
const DUMP: &str = "assets/syntaxes.packdump";
const DIGEST: &str = "assets/syntaxes.packdump.digest";
const OUT_DUMP: &str = "syntaxes.packdump";

/// Set this to regenerate [`DUMP`] and [`DIGEST`] from the vendored grammars.
const REGENERATE: &str = "RADIAN_REGENERATE_SYNTAXES";

/// Part of the digest: the recipe that turns the grammars into the dump. Bump it when the recipe
/// changes (a different syntect, `lines_include_newline`, the list of grammars) — the grammar
/// files themselves are covered by their own bytes.
const RECIPE: &str = "radian-syntax-dump-1: syntect 5.3, load_defaults_newlines + vendored extras";

fn main() {
    // Runtime, not `env!`: the build script's own `CARGO_MANIFEST_DIR` is baked when the
    // build script is compiled, so a workspace moved (renamed) after that kept reading the old
    // absolute path. Cargo sets this variable when it **runs** the build script.
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let grammars = manifest.join("../../third_party/syntaxes");
    let dump = manifest.join(DUMP);
    let digest_file = manifest.join(DIGEST);
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join(OUT_DUMP);
    println!("cargo:rerun-if-changed={}", grammars.display());
    println!("cargo:rerun-if-changed={}", dump.display());
    println!("cargo:rerun-if-changed={}", digest_file.display());
    // Without this, the `REGENERATE` run below would do nothing on a tree whose inputs are
    // unchanged: the `rerun-if-changed` lines above replace cargo's default "rerun when anything
    // in the package changed" rule, and cargo does not watch the environment on its own.
    println!("cargo:rerun-if-env-changed={REGENERATE}");

    let digest = digest(&grammars);
    if env::var_os(REGENERATE).is_some() {
        regenerate(&grammars, &dump, &digest_file, &digest, &out);
        return;
    }
    let committed = std::fs::read_to_string(&digest_file).unwrap_or_default();
    if committed.trim() != digest {
        panic!(
            "the committed syntax dump is stale: {DIGEST} says `{}`, the grammars under {} digest \
             to `{digest}`. Regenerate with `{REGENERATE}=1 cargo build -p radian-tui`, then \
             commit {DUMP} and {DIGEST}.",
            committed.trim(),
            grammars.display(),
        );
    }
    if let Err(error) = syntect::dumps::from_uncompressed_dump_file::<SyntaxSet, _>(&dump) {
        panic!(
            "{DUMP} does not load with this syntect ({error}): regenerate with \
             `{REGENERATE}=1 cargo build -p radian-tui`, then commit {DUMP} and {DIGEST}."
        );
    }
    std::fs::copy(&dump, &out).expect("copy the committed syntax dump into OUT_DIR");
}

/// Writes both committed files from the vendored grammars (the `REGENERATE` path).
fn regenerate(grammars: &Path, dump: &Path, digest_file: &Path, digest: &str, out: &Path) {
    let mut builder = SyntaxSet::load_defaults_newlines().into_builder();
    for (name, file) in EXTRA_SYNTAXES {
        let path = grammars.join(file);
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        let definition = SyntaxDefinition::load_from_str(&source, true, Some(name))
            .unwrap_or_else(|error| panic!("cannot parse {} ({name}): {error}", path.display()));
        builder.add(definition);
    }
    // `into_builder` + `build` re-resolves context references across the whole set, which is why
    // the extras are added before `build` (a subset alone would leave dangling references).
    let set = builder.build();
    syntect::dumps::dump_to_uncompressed_file(&set, out).expect("write the syntax dump");
    std::fs::copy(out, dump)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", dump.display()));
    std::fs::write(digest_file, format!("{digest}\n"))
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", digest_file.display()));
    println!("cargo:warning=regenerated {DUMP} and {DIGEST}: commit them");
}

/// The digest of the inputs the committed dump is generated from: the recipe, then each vendored
/// grammar's name and bytes. FNV-1a 64 — this only has to notice an edit, and it is stable.
fn digest(grammars: &Path) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    hash = fnv1a(RECIPE.as_bytes(), hash);
    for (name, file) in EXTRA_SYNTAXES {
        hash = fnv1a(name.as_bytes(), hash);
        let path = grammars.join(file);
        let source = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        hash = fnv1a(&source, hash);
    }
    format!("fnv1a64:{hash:016x}")
}

fn fnv1a(bytes: &[u8], mut hash: u64) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}
