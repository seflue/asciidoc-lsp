//! `--version` output: the version is stamped at build time (see `build.rs`).

/// Picks the version string: the release override, else `git describe` as
/// semver build metadata, else `<package_version>+unknown`. Empty values count
/// as unset; a leading `v` is dropped. Tags win while one is reachable;
/// `package_version` (the Cargo version, which `just release` keeps equal to
/// the latest tag) is only the base when no tag is.
///
/// A local build prints `X.Y.Z+<n>.<sha>[.dirty]` (`n` commits after the tag),
/// `X.Y.Z[+dirty]` on a tag, and `<package_version>+<sha>[.dirty]` when no tag
/// is reachable.
pub fn select_version(
    override_env: Option<&str>,
    git_describe: Option<&str>,
    package_version: &str,
) -> String {
    if let Some(v) = non_empty(override_env) {
        return strip_v(v).to_owned();
    }
    non_empty(git_describe).map_or_else(
        || format!("{package_version}+unknown"),
        |d| describe_to_semver(d, package_version),
    )
}

fn non_empty(v: Option<&str>) -> Option<&str> {
    v.map(str::trim).filter(|v| !v.is_empty())
}

fn strip_v(v: &str) -> &str {
    v.strip_prefix('v').unwrap_or(v)
}

/// Rewrites `git describe --tags --always --dirty` output as semver build metadata.
fn describe_to_semver(describe: &str, package_version: &str) -> String {
    let describe = strip_v(describe);
    let (rest, dirty) = describe
        .strip_suffix("-dirty")
        .map_or((describe, false), |r| (r, true));
    let after_tag = rest.rsplit_once("-g").and_then(|(head, sha)| {
        let (tag, n) = head.rsplit_once('-')?;
        let is_count = !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
        let is_sha = !sha.is_empty() && sha.bytes().all(|b| b.is_ascii_hexdigit());
        (is_count && is_sha).then_some((tag, n, sha))
    });
    let mut out = if let Some((tag, n, sha)) = after_tag {
        format!("{tag}+{n}.{sha}")
    } else if rest.contains('.') {
        rest.to_owned()
    } else {
        format!("{package_version}+{rest}")
    };
    if dirty {
        out.push(if out.contains('+') { '.' } else { '+' });
        out.push_str("dirty");
    }
    out
}

/// The single line printed by `--version`, without trailing newline.
pub fn version_line(version: &str) -> String {
    format!("asciidoc-lsp {version}")
}

/// The version this binary was built as.
pub fn build_version() -> String {
    select_version(
        option_env!("ASCIIDOC_LSP_BUILD_VERSION"),
        option_env!("ASCIIDOC_LSP_BUILD_GIT_DESCRIBE"),
        env!("CARGO_PKG_VERSION"),
    )
}

/// True when the command-line arguments (without the program name) ask for the version.
pub fn wants_version<I: IntoIterator<Item = S>, S: AsRef<str>>(args: I) -> bool {
    args.into_iter()
        .any(|a| matches!(a.as_ref(), "--version" | "-V"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PKG: &str = "0.9.0";

    fn select(override_env: Option<&str>, git_describe: Option<&str>) -> String {
        select_version(override_env, git_describe, PKG)
    }

    #[test]
    fn test_env_var_wins_over_git_describe() {
        assert_eq!(select(Some("0.7.0"), Some("v0.6.0-12-gabc1234")), "0.7.0");
    }

    #[test]
    fn test_leading_v_is_stripped_from_env_var() {
        assert_eq!(select(Some("v0.7.0"), None), "0.7.0");
    }

    #[test]
    fn test_exact_tag_is_the_bare_version() {
        assert_eq!(select(None, Some("v0.9.0")), "0.9.0");
    }

    #[test]
    fn test_commits_after_tag_become_build_metadata() {
        assert_eq!(select(None, Some("v0.9.0-18-ge63ebcd")), "0.9.0+18.e63ebcd");
        assert_eq!(
            select(None, Some("v0.9.0-18-ge63ebcd-dirty")),
            "0.9.0+18.e63ebcd.dirty"
        );
    }

    #[test]
    fn test_dirty_on_exact_tag() {
        assert_eq!(select(None, Some("v0.9.0-dirty")), "0.9.0+dirty");
    }

    #[test]
    fn test_prerelease_tag_keeps_its_prerelease_part() {
        assert_eq!(
            select(None, Some("v0.10.0-rc.1-3-gabc1234")),
            "0.10.0-rc.1+3.abc1234"
        );
        assert_eq!(select(None, Some("v0.10.0-rc.1")), "0.10.0-rc.1");
    }

    #[test]
    fn test_bare_hash_without_tag_uses_package_version() {
        assert_eq!(select(None, Some("e63ebcd")), "0.9.0+e63ebcd");
        assert_eq!(select(None, Some("e63ebcd-dirty")), "0.9.0+e63ebcd.dirty");
        assert_eq!(
            select_version(None, Some("e63ebcd"), "1.2.3"),
            "1.2.3+e63ebcd"
        );
    }

    #[test]
    fn test_empty_values_fall_through() {
        assert_eq!(select(Some(" "), Some("abc1234")), "0.9.0+abc1234");
        assert_eq!(select(Some(""), Some("")), "0.9.0+unknown");
    }

    #[test]
    fn test_fallback_is_package_version_unknown() {
        assert_eq!(select(None, None), "0.9.0+unknown");
    }

    #[test]
    fn test_version_line_format() {
        assert_eq!(version_line("0.7.0"), "asciidoc-lsp 0.7.0");
    }

    #[test]
    fn test_wants_version_flags() {
        assert!(wants_version(["--version"]));
        assert!(wants_version(["-V"]));
        assert!(!wants_version(["--stdio"]));
        assert!(!wants_version(Vec::<String>::new()));
    }

    #[test]
    fn test_build_version_is_non_empty_and_has_no_leading_v() {
        let v = build_version();
        assert!(!v.is_empty());
        assert!(!v.starts_with('v'));
    }
}
