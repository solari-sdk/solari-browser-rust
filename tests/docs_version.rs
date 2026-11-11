//! The dependency line shown on docs.rs (the crate docs in src/lib.rs) and on crates.io (README.md) must name a version this
//! crate satisfies. docs.rs renders src/lib.rs, not the README, so a stale `"0.2"` there stayed on the published docs after the
//! README was fixed (0.6.4) and told users to depend on a release that cannot be fetched.

fn dependency_versions(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let l = line.trim_start_matches(['/', '!', ' ']);
        if let Some(rest) = l.strip_prefix("solari-browser = { version = \"") {
            if let Some(end) = rest.find('"') {
                out.push(rest[..end].to_string());
            }
        }
    }
    out
}

/// "0.6" accepts 0.6.x; "0.6.4" accepts exactly that or later 0.6.x patch (a prefix of the package version's major.minor).
fn satisfied_by(req: &str, pkg: &str) -> bool {
    let want: Vec<&str> = req.split('.').collect();
    let have: Vec<&str> = pkg.split('.').collect();
    want.len() <= 3 && want.iter().zip(&have).take(2).all(|(a, b)| a == b) && want.len() >= 2
}

#[test]
fn documented_dependency_versions_match_the_crate() {
    let pkg = env!("CARGO_PKG_VERSION");
    for (file, text) in [
        ("README.md", include_str!("../README.md")),
        ("src/lib.rs", include_str!("../src/lib.rs")),
    ] {
        let found = dependency_versions(text);
        assert!(!found.is_empty(), "{file} shows no `solari-browser = {{ version = ... }}` line");
        for v in found {
            assert!(satisfied_by(&v, pkg), "{file} documents solari-browser = \"{v}\" but this crate is {pkg}");
        }
    }
}

#[test]
fn the_version_matcher_rejects_a_stale_requirement() {
    assert!(satisfied_by("0.6", "0.6.5"));
    assert!(satisfied_by("0.6.5", "0.6.5"));
    assert!(!satisfied_by("0.2", "0.6.5"));
    assert!(!satisfied_by("0", "0.6.5"));
}
