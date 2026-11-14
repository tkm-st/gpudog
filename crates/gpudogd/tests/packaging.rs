//! Checks on the Debian packaging that a version bump could break.

/// Read at build time, so the test also runs where only the files the
/// release image copies before `cargo test` exist.
const CHANGELOG: &str = include_str!("../../../packaging/debian/changelog");

/// The deb version as cargo-deb 3.8 writes it (`manifest_version_string`):
/// a prerelease with both digits and other characters, such as
/// `0.1.0-rc.1`, becomes `0.1.0~rc.1` so that it sorts before `0.1.0`;
/// then the default revision `-1` is appended.
fn deb_version(version: &str) -> String {
    let version = match version.split_once('-') {
        Some((main, pre))
            if pre.bytes().any(|c| !c.is_ascii_digit())
                && pre.bytes().any(|c| c.is_ascii_digit()) =>
        {
            format!("{main}~{pre}")
        }
        _ => version.to_owned(),
    };
    format!("{version}-1")
}

#[test]
fn deb_versions_follow_cargo_deb() {
    assert_eq!(deb_version("0.0.1"), "0.0.1-1");
    assert_eq!(deb_version("0.0.2-rc.1"), "0.0.2~rc.1-1");
    assert_eq!(deb_version("0.0.2-rc"), "0.0.2-rc-1");
}

#[test]
fn the_debian_changelog_starts_with_this_version() {
    let version = deb_version(env!("CARGO_PKG_VERSION"));
    let expected = format!("gpudog ({version}) unstable; ");
    assert!(
        CHANGELOG.starts_with(&expected),
        "packaging/debian/changelog must start with {expected:?} (dch writes UNRELEASED until `dch -r`)"
    );
}
