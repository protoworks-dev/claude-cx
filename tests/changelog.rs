//! CHANGELOG.md follows Keep a Changelog 1.1.0, and its newest version is the
//! crate's version. Checked here so that a release cannot go out with a
//! changelog that tools and people cannot read the usual way.

const KINDS: [&str; 6] = ["Added", "Changed", "Deprecated", "Removed", "Fixed", "Security"];

fn is_version(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

fn is_date(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

#[test]
fn the_changelog_follows_keep_a_changelog() {
    let text = include_str!("../CHANGELOG.md");
    assert!(text.starts_with("# Changelog\n"), "the first line is `# Changelog`");
    assert!(text.contains("https://keepachangelog.com/en/1.1.0/") && text.contains("https://semver.org/"));

    let mut versions: Vec<String> = Vec::new();
    let mut kinds: Vec<&str> = Vec::new();
    let mut entries = 0;
    let mut section_has_entry = true;
    for line in text.lines() {
        if let Some(title) = line.strip_prefix("## ") {
            assert!(section_has_entry, "a section before `{title}` has no entry");
            kinds.clear();
            if title == "[Unreleased]" {
                assert!(versions.is_empty(), "[Unreleased] comes first");
                versions.push("Unreleased".into());
                continue;
            }
            let (version, date) = title
                .split_once(" - ")
                .unwrap_or_else(|| panic!("`## {title}`: want `## [x.y.z] - YYYY-MM-DD`"));
            let version = version
                .strip_prefix('[')
                .and_then(|v| v.strip_suffix(']'))
                .unwrap_or_else(|| panic!("`## {title}`: the version goes in [brackets]"));
            assert!(is_version(version), "`{version}` is not x.y.z");
            assert!(is_date(date), "`{date}` is not YYYY-MM-DD");
            versions.push(version.to_string());
        } else if let Some(kind) = line.strip_prefix("### ") {
            assert!(section_has_entry, "the section before `### {kind}` has no entry");
            assert!(KINDS.contains(&kind), "`### {kind}` is not one of {KINDS:?}");
            assert!(
                !kinds.contains(&kind),
                "`### {kind}` appears twice under {:?}",
                versions.last()
            );
            let at = |k: &str| KINDS.iter().position(|x| *x == k).unwrap();
            assert!(
                kinds.last().is_none_or(|last| at(last) < at(kind)),
                "`### {kind}` is out of order: {KINDS:?}"
            );
            kinds.push(kind);
            section_has_entry = false;
        } else if line.starts_with("- ") {
            assert!(!kinds.is_empty(), "an entry outside a `###` section: {line}");
            section_has_entry = true;
            entries += 1;
        }
    }
    assert!(section_has_entry && entries > 0);
    assert_eq!(
        versions.first().map(String::as_str),
        Some("Unreleased"),
        "there is an [Unreleased] section"
    );
    assert_eq!(
        versions.get(1).map(String::as_str),
        Some(env!("CARGO_PKG_VERSION")),
        "the newest version is the crate's version"
    );

    // Newest first, and every heading has its link at the foot of the file.
    let numbers = |v: &str| -> Vec<u32> { v.split('.').map(|p| p.parse().unwrap()).collect() };
    for pair in versions[1..].windows(2) {
        assert!(
            numbers(&pair[0]) > numbers(&pair[1]),
            "{} must come before {}",
            pair[0],
            pair[1]
        );
    }
    for version in &versions {
        let link = format!("\n[{version}]: https://github.com/protoworks-dev/claude-cx/");
        assert!(text.contains(&link), "no link for [{version}] at the foot of the file");
    }
}
