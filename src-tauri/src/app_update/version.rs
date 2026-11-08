//! Lavender's release versions. The app version is upstream's (`2.2.0`, from
//! Cargo.toml) because MSI needs a purely numeric version; the fork's release
//! number lives in the tag (`v2.2.0-local.3`). Release builds learn that number
//! from the `LAVENDER_RELEASE` environment variable at compile time (set by
//! `release.yml` from the tag). A build without it counts as release 0, so any
//! published release of the same base version is newer.

use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LocalVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub local: u64,
}

/// Parses `2.2.0`, `v2.2.0` and `v2.2.0-local.3`. Anything else (another
/// pre-release label, build metadata, missing parts) is rejected, so an
/// unexpected tag is never taken for a newer release.
pub fn parse(text: &str) -> Option<LocalVersion> {
    let text = text.trim();
    let text = text.strip_prefix('v').unwrap_or(text);
    let (base, local) = match text.split_once('-') {
        Some((base, suffix)) => (base, Some(suffix.strip_prefix("local.")?)),
        None => (text, None),
    };
    let mut parts = base.split('.');
    let major = number(parts.next()?)?;
    let minor = number(parts.next()?)?;
    let patch = number(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    let local = match local {
        Some(n) => number(n)?,
        None => 0,
    };
    Some(LocalVersion {
        major,
        minor,
        patch,
        local,
    })
}

fn number(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The running build's version, e.g. `2.2.0-local.3`, or `2.2.0` for a build
/// made without `LAVENDER_RELEASE`.
pub fn current() -> String {
    with_release(env!("CARGO_PKG_VERSION"), option_env!("LAVENDER_RELEASE"))
}

fn with_release(base: &str, release: Option<&str>) -> String {
    match release.map(str::trim).filter(|n| number(n).is_some()) {
        Some(n) => format!("{base}-local.{n}"),
        None => base.to_string(),
    }
}

/// True only when both versions parse and `latest` is strictly newer.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse(latest), parse(current)) {
        (Some(latest), Some(current)) => latest.cmp(&current) == Ordering::Greater,
        _ => false,
    }
}

/// The version without a leading `v`, for display.
pub fn display(text: &str) -> &str {
    let text = text.trim();
    text.strip_prefix('v').unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_tag_forms() {
        let v = parse("v2.2.0-local.3").unwrap();
        assert_eq!((v.major, v.minor, v.patch, v.local), (2, 2, 0, 3));
        assert_eq!(parse("2.2.0").unwrap().local, 0);
        assert_eq!(parse("v2.2.0"), parse("2.2.0-local.0"));
    }

    #[test]
    fn rejects_unexpected_forms() {
        for bad in [
            "",
            "v2.2",
            "2.2.0.1",
            "2.2.0-rc.1",
            "2.2.0-local.",
            "2.2.0-local.x",
            "2.2.0+local.1",
            "latest",
            "2.-1.0",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn compares_base_first_then_the_release_number() {
        assert!(is_newer("v2.2.0-local.2", "2.2.0-local.1"));
        assert!(is_newer("v2.2.0-local.1", "2.2.0"));
        assert!(is_newer("v2.3.0-local.1", "2.2.0-local.9"));
        assert!(is_newer("v2.2.10-local.1", "2.2.9-local.4"));
        assert!(!is_newer("v2.2.0-local.1", "2.2.0-local.1"));
        assert!(!is_newer("v2.2.0-local.1", "2.2.0-local.2"));
        assert!(!is_newer("v2.1.9-local.9", "2.2.0-local.1"));
    }

    #[test]
    fn an_unparseable_version_is_never_newer() {
        assert!(!is_newer("v9.9.9-beta", "2.2.0-local.1"));
        assert!(!is_newer("v2.2.0-local.2", "nonsense"));
    }

    #[test]
    fn the_current_version_carries_the_release_number() {
        assert_eq!(with_release("2.2.0", Some("4")), "2.2.0-local.4");
        assert_eq!(with_release("2.2.0", None), "2.2.0");
        assert_eq!(with_release("2.2.0", Some("")), "2.2.0");
        assert_eq!(with_release("2.2.0", Some("x1")), "2.2.0");
        assert!(parse(&current()).is_some());
    }

    #[test]
    fn display_drops_the_v() {
        assert_eq!(display("v2.2.0-local.1"), "2.2.0-local.1");
        assert_eq!(display("2.2.0"), "2.2.0");
    }
}
