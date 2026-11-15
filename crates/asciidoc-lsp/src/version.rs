//! `--version` output: the version is stamped at build time (see `build.rs`).

/// Picks the version string: the release override, else `git describe`,
/// else `<pkg>-unknown`. Empty values count as unset; a leading `v` is dropped.
pub fn select_version(override_env: Option<&str>, git_describe: Option<&str>, pkg: &str) -> String {
    [override_env, git_describe]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|v| !v.is_empty())
        .map_or_else(
            || format!("{pkg}-unknown"),
            |v| v.strip_prefix('v').unwrap_or(v).to_owned(),
        )
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

    #[test]
    fn test_env_var_wins_over_git_describe() {
        assert_eq!(
            select_version(Some("0.7.0"), Some("0.6.0-12-gabc1234"), "0.1.0"),
            "0.7.0"
        );
    }

    #[test]
    fn test_leading_v_is_stripped_from_both_sources() {
        assert_eq!(select_version(Some("v0.7.0"), None, "0.1.0"), "0.7.0");
        assert_eq!(
            select_version(None, Some("v0.6.0-12-gabc1234"), "0.1.0"),
            "0.6.0-12-gabc1234"
        );
    }

    #[test]
    fn test_git_describe_used_without_env_var() {
        assert_eq!(
            select_version(None, Some("0.6.0-12-gabc1234-dirty"), "0.1.0"),
            "0.6.0-12-gabc1234-dirty"
        );
    }

    #[test]
    fn test_empty_values_fall_through() {
        assert_eq!(
            select_version(Some(" "), Some("abc1234"), "0.1.0"),
            "abc1234"
        );
        assert_eq!(select_version(Some(""), Some(""), "0.1.0"), "0.1.0-unknown");
    }

    #[test]
    fn test_fallback_is_pkg_version_unknown() {
        assert_eq!(select_version(None, None, "0.1.0"), "0.1.0-unknown");
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
