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
use std::time::Duration;

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
///
/// Both fields are held in an `Option` so [`Sismatic::close`] can tear the
/// session down deterministically — while the interpreter is still alive —
/// rather than leaving a live multi-threaded runtime and warm SSH session to be
/// dropped during `Py_Finalize`, where a russh worker thread logging through
/// pyo3-log into the finalizing interpreter segfaults the process.
///
/// Declaration order is load-bearing for the default (no-`close`) drop path:
/// `registry` is listed first so it drops *before* `runtime`, closing the SSH
/// connections while the reactor is still running instead of after it is gone.
///
/// `weakref` is enabled so [`from_toml`] can hand a weak reference to an
/// `atexit` hook, making even a bare `Sis.from_toml(...)` — no `with`, no manual
/// `close` — tear down before finalization without pinning the session alive.
#[pyclass(name = "Sis", weakref)]
struct Sismatic {
    registry: Option<Registry>,
    runtime: Option<Runtime>,
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
    fn from_toml(py: Python<'_>, path: &str) -> PyResult<Py<Self>> {
        let runtime = Runtime::new().map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        let registry = Registry::load(path, Arc::new(RusshConnector))
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let session = Py::new(
            py,
            Self {
                registry: Some(registry),
                runtime: Some(runtime),
            },
        )?;
        register_atexit_close(py, &session)?;
        Ok(session)
    }

    /// The ids of every configured device.
    fn ids(&self) -> PyResult<Vec<String>> {
        Ok(self.registry()?.ids())
    }

    /// Close every SSH connection and shut the tokio runtime down, in that
    /// order, while the interpreter is still alive. The GIL is released for the
    /// duration so russh's worker threads can keep logging through pyo3-log as
    /// they wind down. Idempotent: a second call (or a call after a `with`
    /// block) is a no-op, and any method used afterwards raises `RuntimeError`.
    fn close(&mut self, py: Python<'_>) {
        let registry = self.registry.take();
        let runtime = self.runtime.take();
        py.detach(move || match runtime {
            Some(runtime) => {
                // Drop the connections inside the runtime context so russh's
                // Drop impls can reach the still-running session tasks, then
                // give those tasks a bounded moment to close cleanly.
                {
                    let _enter = runtime.enter();
                    drop(registry);
                }
                runtime.shutdown_timeout(Duration::from_secs(5));
            }
            None => drop(registry),
        });
    }

    /// Enter a `with` block; the session is returned unchanged.
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Leave a `with` block, closing the session. Returns `False` so any
    /// in-flight exception continues to propagate.
    fn __exit__(
        &mut self,
        py: Python<'_>,
        _exc_type: &Bound<'_, PyAny>,
        _exc_value: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) -> bool {
        self.close(py);
        false
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
    /// Borrow the registry, or raise if the session has been closed.
    fn registry(&self) -> PyResult<&Registry> {
        self.registry.as_ref().ok_or_else(closed_err)
    }

    /// Borrow the runtime, or raise if the session has been closed.
    fn runtime(&self) -> PyResult<&Runtime> {
        self.runtime.as_ref().ok_or_else(closed_err)
    }

    /// Look up `device`, run `instruction` to completion on the runtime, and turn
    /// the decoded value into a native Python object. The GIL is released for the
    /// duration of the (blocking) device exchange.
    fn execute(
        &self,
        py: Python<'_>,
        device: &str,
        instruction: Instruction,
    ) -> PyResult<Py<PyAny>> {
        let registry = self.registry()?;
        let runtime = self.runtime()?;
        let device = registry
            .device(device)
            .ok_or_else(|| PyValueError::new_err(format!("unknown device '{device}'")))?;

        let value = py
            .detach(|| runtime.block_on(device.run(&instruction)))
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

        value_into_py(py, value)
    }
}

/// The error raised when a method is used after the session has been closed.
fn closed_err() -> PyErr {
    PyRuntimeError::new_err("Sis session is closed")
}

/// Arrange for `session.close()` to run at interpreter shutdown, so a session
/// that is neither used in a `with` block nor closed by hand is still torn down
/// deterministically *before* finalization — never during it, where a russh
/// worker thread logging through pyo3-log would segfault the process (see
/// [`Sismatic::close`]). The hook is handed only a weak reference, so it never
/// keeps the session alive and quietly does nothing if it was already dropped.
fn register_atexit_close(py: Python<'_>, session: &Py<Sismatic>) -> PyResult<()> {
    let session_ref = py
        .import("weakref")?
        .getattr("ref")?
        .call1((session.bind(py),))?;
    let hook = pyo3::wrap_pyfunction!(close_session_at_exit, py)?;
    py.import("atexit")?
        .getattr("register")?
        .call1((&hook, &session_ref))?;
    Ok(())
}

/// The `atexit` callback: dereference the weak reference and, if the session is
/// still alive, close it. A no-op once the session has been garbage-collected or
/// already closed (`close` is idempotent).
#[pyfunction]
fn close_session_at_exit(session_ref: &Bound<'_, PyAny>) -> PyResult<()> {
    let session = session_ref.call0()?;
    if !session.is_none() {
        session.call_method0("close")?;
    }
    Ok(())
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
