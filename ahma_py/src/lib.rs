//! # Ahma Python Bindings
//!
//! This crate exposes `ahma_core` to Python via PyO3.  It is the foundation
//! for an `ahma-py` wheel that lets Jupyter notebooks, FastAPI services, and
//! Streamlit apps embed Ahma's secure sandboxing in their own Rust application.
//!
//! ## License
//!
//! This crate is licensed under **MIT OR Apache-2.0**.
//!
//! ## AGPL-licensed bindings
//!
//! Decompose, vault, worker, and cluster primitives are implemented in separate
//! AGPL-licensed crates (`ahma_vault`, `ahma_decompose`, etc.).  Python bindings
//! for those features are planned for a future `ahma_py_agpl` wheel that will be
//! distributed separately under AGPL-3.0-or-later terms.
//!
//! ## Building the wheel
//!
//! ```bash
//! pip install maturin
//! maturin develop --features python -m ahma_py/Cargo.toml
//! maturin build --release --features python -m ahma_py/Cargo.toml
//! ```

/// Synchronously create a task vault (blocking wrapper for Python sync code).
pub fn create_vault_sync(slug: &str) -> anyhow::Result<std::path::PathBuf> {
    // TaskVault is still accessible via ahma_vault if that AGPL crate is added.
    // This stub shows the intended interface.
    let _ = slug;
    anyhow::bail!(
        "create_vault_sync requires ahma_vault (AGPL-3.0-or-later). \
         Add ahma_vault to your Cargo.toml if you accept AGPL terms."
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// PyO3 module (only when --features python is set)
// ─────────────────────────────────────────────────────────────────────────────
//
// When pyo3 is added to the workspace dependencies, uncomment the block below
// and activate with `--features python`.
//
// #[cfg(feature = "python")]
// mod python_module {
//     use pyo3::prelude::*;
//     use super::*;
//
//     #[pymodule]
//     pub fn ahma_py(_py: Python, m: &PyModule) -> PyResult<()> {
//         Ok(())
//     }
// }

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_vault_sync_without_ahma_vault_errors() {
        let result = create_vault_sync("test-python-binding");
        assert!(result.is_err(), "expected error without ahma_vault AGPL crate");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("AGPL"), "error should mention AGPL");
    }
}
