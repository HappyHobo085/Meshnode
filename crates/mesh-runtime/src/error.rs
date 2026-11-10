//! Errors from creating the runtime and loading components.

/// Why a runtime could not be created or a component could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// The component does not fit the `meshnode:service/service` world: it imports
    /// something outside the world or does not export `wasi:http/incoming-handler`.
    #[error("invalid component: {0}")]
    Invalid(String),
    /// The bytes could not be compiled (not a component, malformed, unsupported features).
    #[error("compile error: {0}")]
    Compile(String),
    /// Reading or writing the compiled-code cache failed.
    #[error("code cache: {0}")]
    Cache(#[from] std::io::Error),
}
