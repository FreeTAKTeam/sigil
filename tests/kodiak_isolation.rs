//! Kodiak packages must stay independent of Sigil so they can move to
//! FreeTAKTeam/R3AKT-Kodiak unchanged.

use std::fs;
use std::path::{Path, PathBuf};

const PACKAGES: [&str; 4] = ["kodiak-core", "kodiak-asr", "kodiak-intent", "kodiak-laya"];

/// Crates that belong to Sigil's radio, data and publishing layers.
const FORBIDDEN: [&str; 13] = [
    "r3akt-sigil",
    "r3akt_sigil",
    "lxmf",
    "rns-transport",
    "reticulum",
    "librtlsdr",
    "rtl-sdr-rs",
    "rtl_sdr_rs",
    "rusb",
    "sdr-dsp",
    "sdr-types",
    "yodel",
    "r3akt-map-core",
];

fn files(directory: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files(&path, found);
        } else {
            found.push(path);
        }
    }
}

#[test]
fn kodiak_packages_do_not_depend_on_sigil() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for package in PACKAGES {
        let directory = root.join("crates").join(package);
        let manifest = fs::read_to_string(directory.join("Cargo.toml")).unwrap();
        assert!(
            !manifest.lines().any(
                |line| !line.trim_start().starts_with('#') && line.contains("workspace = true")
            ),
            "{package} inherits workspace settings"
        );
        for line in manifest.lines().filter(|line| line.contains("path =")) {
            let path = line
                .split("path =")
                .nth(1)
                .unwrap()
                .trim()
                .trim_matches(|c| c == '"' || c == '}' || c == ' ');
            assert!(
                path.starts_with("../kodiak-"),
                "{package} has a non-Kodiak path dependency: {line}"
            );
        }
        let mut sources = Vec::new();
        files(&directory, &mut sources);
        for source in sources.iter().filter(|path| {
            path.extension()
                .is_some_and(|ext| ext == "rs" || ext == "toml")
        }) {
            let text = fs::read_to_string(source).unwrap();
            for forbidden in FORBIDDEN {
                let used = text.lines().any(|line| {
                    let line = line.trim_start();
                    !line.starts_with("//") && !line.starts_with('#') && line.contains(forbidden)
                });
                assert!(!used, "{} references {forbidden}", source.display());
            }
            for include in text
                .match_indices("include_str!(\"")
                .map(|(at, _)| &text[at..])
            {
                assert!(
                    !include.contains("\"../../"),
                    "{} includes a file outside its package",
                    source.display()
                );
            }
        }
    }
}
