//! Launcher-owned MCP authority mode (`LEAN_CTX_AUTHORITY_MODE`).
//!
//! A trusted launcher (Harr) fixes the mode before exec; the value is parsed
//! once per process and is immutable afterwards. Direct LeanCTX clients that
//! launch without the variable keep the prior behavior ([`AuthorityMode::Legacy`]).
//! No tool argument, MCP root, client capability or environment change may
//! move a running server between modes.

/// Authority scope of one MCP server process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AuthorityMode {
    /// No launcher mode: direct-client compatibility behavior (cwd/roots may
    /// seed session state as before).
    #[default]
    Legacy,
    /// Rootless: cwd, `.`, tool arguments, client roots and extra-root env
    /// never grant read/write authority.
    None,
    /// Launcher-bound Workspace file plus strict request admission.
    Workspace,
}

pub(crate) const AUTHORITY_MODE_ENV: &str = "LEAN_CTX_AUTHORITY_MODE";

impl AuthorityMode {
    /// Parse the launcher variable. Absent = [`AuthorityMode::Legacy`]; an
    /// unknown or empty value is an error the caller must treat as fatal.
    pub fn parse_env() -> Result<Self, String> {
        Self::parse_os_value(std::env::var_os(AUTHORITY_MODE_ENV))
    }

    fn parse_os_value(value: Option<std::ffi::OsString>) -> Result<Self, String> {
        let Some(value) = value else {
            return Ok(Self::Legacy);
        };
        let value = value
            .into_string()
            .map_err(|_| format!("{AUTHORITY_MODE_ENV} must be valid UTF-8 when set"))?;
        Self::parse_value(&value)
    }

    /// Parse a concrete value (`workspace` | `none`); empty and unknown error.
    pub fn parse_value(value: &str) -> Result<Self, String> {
        match value.trim() {
            "" => Err(format!(
                "{AUTHORITY_MODE_ENV} must be `workspace` or `none` when set, got an empty value"
            )),
            "workspace" => Ok(Self::Workspace),
            "none" => Ok(Self::None),
            other => Err(format!(
                "{AUTHORITY_MODE_ENV} must be `workspace` or `none`, got `{other}`"
            )),
        }
    }

    /// Wire value of the mode; `None` when the mode must not be re-exported
    /// (legacy keeps the launcher's original environment untouched).
    pub fn env_value(self) -> Option<&'static str> {
        match self {
            Self::Legacy => None,
            Self::None => Some("none"),
            Self::Workspace => Some("workspace"),
        }
    }

    /// True for the rootless scope: no implicit authority source at all.
    pub fn is_rootless(self) -> bool {
        self == Self::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_value_accepts_only_contract_values() {
        assert_eq!(
            AuthorityMode::parse_value("workspace"),
            Ok(AuthorityMode::Workspace)
        );
        assert_eq!(AuthorityMode::parse_value("none"), Ok(AuthorityMode::None));
        assert!(AuthorityMode::parse_value("").is_err());
        assert!(AuthorityMode::parse_value("strict").is_err());
        assert!(AuthorityMode::parse_value(" workspace ").is_ok());
    }

    #[test]
    fn parse_env_absent_is_legacy() {
        crate::test_env::remove_var(AUTHORITY_MODE_ENV);
        assert_eq!(AuthorityMode::parse_env(), Ok(AuthorityMode::Legacy));
    }

    #[test]
    fn parse_env_rejects_unknown_mode() {
        let _guard = crate::setup::EnvVarGuard::set(AUTHORITY_MODE_ENV, "bogus");
        assert!(AuthorityMode::parse_env().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn parse_os_value_rejects_non_unicode_mode() {
        use std::os::unix::ffi::OsStringExt;

        let value = std::ffi::OsString::from_vec(vec![0xff]);
        assert!(AuthorityMode::parse_os_value(Some(value)).is_err());
    }
}
