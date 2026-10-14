//! Small helpers.

/// Expands a leading `~/` to the home directory.
pub fn expand_tilde(path: &str) -> String {
    tableplus_core::tunnel::expand_tilde(path).display().to_string()
}
