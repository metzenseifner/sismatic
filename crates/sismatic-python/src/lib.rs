//! Python packaging for sismatic: the type-stub generator plus, behind the
//! `python` feature, the compiled pyo3 extension module.
//!
//! The Rust domain lives in [`sismatic_core`]; this crate only adapts it for
//! Python consumers.

pub mod stub;

#[cfg(feature = "python")]
mod python_binding;
