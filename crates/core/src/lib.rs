//! Hearth core: the Rust side that every front end shares.

/// The version of Hearth, as set in the workspace manifest.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fails if version() returns an empty or non-semver-shaped string.
    #[test]
    fn version_is_dotted_numbers() {
        let parts: Vec<&str> = version().split('.').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.parse::<u32>().is_ok()));
    }
}
