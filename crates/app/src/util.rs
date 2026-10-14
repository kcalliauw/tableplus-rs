//! Small helpers.

/// Expands a leading `~/` to the home directory.
pub fn expand_tilde(path: &str) -> String {
    tableplus_core::tunnel::expand_tilde(path).display().to_string()
}

/// Label for "remember this password"; only macOS stores it in the Keychain.
#[cfg(target_os = "macos")]
pub const SAVE_PASSWORD_LABEL: &str = "Save in Keychain";
#[cfg(not(target_os = "macos"))]
pub const SAVE_PASSWORD_LABEL: &str = "Save password";
