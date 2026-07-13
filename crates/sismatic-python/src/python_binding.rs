//! The blocking Python facade.
//!
//! Python callers see one class, [`Sismatic`], built from a `devices.toml`. It owns a
//! tokio runtime and the device [`Registry`] (over the real SSH connector), and
//! exposes plain blocking methods: name an instruction as a string, name the
//! target device, get a native Python value back. The async machinery and the
//! warm-connection cache live entirely on the Rust side — Python never sees a
//! future, a connection, or an event loop.
//!
//! Each call releases the GIL (`allow_threads`) around `block_on`, so a slow
//! device does not stall other Python threads.

use std::str::FromStr;
use std::sync::Arc;

use pyo3::IntoPyObjectExt;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use tokio::runtime::Runtime;

use sismatic_core::devices::registry::Registry;
use sismatic_core::devices::transport::ssh::RusshConnector;
use sismatic_core::protocol::Value;
use sismatic_core::protocol::instructions::Instruction;
use sismatic_core::protocol::instructions::commands::Command;
use sismatic_core::protocol::instructions::query::Query;
use sismatic_core::protocol::instructions::register::Register;

/// A pool of Extron devices, addressable from Python by id.
#[pyclass(name = "Sis")]
struct Sismatic {
    runtime: Runtime,
    registry: Registry,
}

/// One active alarm, exposed to Python with `.name` and `.level` attributes.
#[pyclass(name = "Alarm", frozen, get_all)]
#[derive(Clone)]
struct Alarm {
    name: String,
    level: String,
}

#[pymethods]
impl Alarm {
    fn __repr__(&self) -> String {
        format!("Alarm(name={:?}, level={:?})", self.name, self.level)
    }
}

#[pymethods]
impl Sismatic {
    /// Build a session from a `devices.toml`, opening no connections yet (each
    /// device connects lazily on its first command).
    #[staticmethod]
    fn from_toml(path: &str) -> PyResult<Self> {
        let runtime = Runtime::new().map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        let registry = Registry::load(path, Arc::new(RusshConnector))
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(Self { runtime, registry })
    }

    /// The ids of every configured device.
    fn ids(&self) -> Vec<String> {
        self.registry.ids()
    }

    /// Read a built-in field (e.g. `"firmware"`, `"ssh_port"`) from `device`.
    fn query(&self, py: Python<'_>, device: &str, name: &str) -> PyResult<Py<PyAny>> {
        let query = Query::from_str(name).map_err(|e| PyValueError::new_err(e.to_string()))?;
        self.execute(py, device, query.instruction())
    }

    /// Run a recorder command (e.g. `"start"`, `"stop"`, `"pause"`) on `device`.
    fn command(&self, py: Python<'_>, device: &str, name: &str) -> PyResult<Py<PyAny>> {
        let command = Command::from_str(name).map_err(|e| PyValueError::new_err(e.to_string()))?;
        self.execute(py, device, command.instruction())
    }

    /// Write `value` into a metadata register (e.g. `"title"`) on `device`. The
    /// device truncates the value at its own length limit.
    fn register(
        &self,
        py: Python<'_>,
        device: &str,
        name: &str,
        value: &str,
    ) -> PyResult<Py<PyAny>> {
        let register =
            Register::from_str(name).map_err(|e| PyValueError::new_err(e.to_string()))?;
        self.execute(py, device, register.instruction(value))
    }
}

impl Sismatic {
    /// Look up `device`, run `instruction` to completion on the runtime, and turn
    /// the decoded value into a native Python object. The GIL is released for the
    /// duration of the (blocking) device exchange.
    fn execute(
        &self,
        py: Python<'_>,
        device: &str,
        instruction: Instruction,
    ) -> PyResult<Py<PyAny>> {
        let device = self
            .registry
            .device(device)
            .ok_or_else(|| PyValueError::new_err(format!("unknown device '{device}'")))?;

        let value = py
            .detach(|| self.runtime.block_on(device.run(&instruction)))
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

        value_into_py(py, value)
    }
}

/// Map a decoded [`Value`] onto the natural Python type: ports/numbers become
/// `int`, flags become `bool`, active alarms become a `list[Alarm]`, and
/// everything else falls back to its string rendering (text, version, ack
/// token, MAC address, recording state).
fn value_into_py(py: Python<'_>, value: Value) -> PyResult<Py<PyAny>> {
    match value {
        Value::Port(p) => p.into_py_any(py),
        Value::Number(n) => n.into_py_any(py),
        Value::Flag(b) => b.into_py_any(py),
        Value::Alarms(a) => a
            .into_iter()
            .map(|(name, level)| Alarm { name, level })
            .collect::<Vec<_>>()
            .into_py_any(py),
        other => other.to_string().into_py_any(py),
    }
}

/// The `sismatic` Python extension module.
#[pymodule]
fn sismatic(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Bridge Rust's `log` output (russh's SSH handshake and auth negotiation)
    // into Python's `logging`; callers see it with
    // `logging.basicConfig(level=logging.DEBUG)`. Ignore the error if a logger
    // is somehow already installed for this process.
    let _ = pyo3_log::init();
    m.add_class::<Sismatic>()?;
    m.add_class::<Alarm>()?;
    Ok(())
}
