//! Debug/trace switches read from the environment.
//!
//! These switches are consulted from hot paths (per block, per feature, per RNG draw),
//! so the environment is read once per call site and the answer is cached. Variable
//! names and their meaning are unchanged; a variable that is set after the first
//! check at a given site is not noticed (nothing sets them at run time).

/// `true` when the environment variable is set to a value readable as UTF-8
/// (the same condition as `std::env::var(name).is_ok()`), cached per call site.
macro_rules! env_flag {
    ($name:literal) => {{
        static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *CACHE.get_or_init(|| std::env::var($name).is_ok())
    }};
}

/// The environment variable's value (`std::env::var(name).ok()`), cached per call site.
macro_rules! env_str {
    ($name:literal) => {{
        static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        CACHE.get_or_init(|| std::env::var($name).ok()).as_deref()
    }};
}

pub(crate) use env_flag;
pub(crate) use env_str;
