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

// TODO Consider using Rust’s documentation blocks but with Sphinx’s RST syntax
// see demo: https://github.com/insight-platform/pyo3-sphinx-documentation

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use pyo3::IntoPyObjectExt;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use pythonize::depythonize;
use tokio::runtime::Runtime;

use sismatic_core::devices::config::{RawConfig, Resolved, resolve_config};
use sismatic_core::devices::registry::{Registry, Target};
use sismatic_core::devices::sis_keepalive::SisKeepalive;
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
/// `sis_keepalive` is listed first so its background tasks are aborted before the
/// `registry` drops, and `registry` before `runtime`, so the SSH connections
/// close while the reactor is still running instead of after it is gone.
///
/// `weakref` is enabled so [`from_toml`] can hand a weak reference to an
/// `atexit` hook, making even a bare `Sis.from_toml(...)` — no `with`, no manual
/// `close` — tear down before finalization without pinning the session alive.
#[pyclass(name = "Sis", weakref)]
struct Sismatic {
    sis_keepalive: Option<SisKeepalive>,
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
    /// Build a session from a config file. Retained for backwards
    /// compatibility: it is exactly [`from_file`](Self::from_file) under an older
    /// name, so it already accepts any supported extension, not just `.toml`.
    #[staticmethod]
    fn from_toml(py: Python<'_>, path: &str) -> PyResult<Py<Self>> {
        Self::from_file(py, path)
    }

    /// Build a session from a config file, choosing the deserializer from the
    /// extension (`.toml`, `.json`, `.yaml`/`.yml`). Devices are connected lazily
    /// on their first command by default; any device marked `eager` in the config
    /// is connected at once and kept warm by a background SIS keepalive, which also
    /// retries the connection on the `eager_retry_secs` interval whenever the device
    /// is cold (unreachable at startup or dropped since).
    #[staticmethod]
    fn from_file(py: Python<'_>, path: &str) -> PyResult<Py<Self>> {
        let resolved = sismatic_core::devices::config::load(path)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        Self::build(py, resolved)
    }

    /// Build a session from an already-parsed mapping shaped like the config
    /// file — a `defaults` table plus a `device` list. Parse the bytes with any
    /// library you like (INI, XML, a database row, environment variables) and
    /// hand the resulting `dict` here; it is deserialized into the core's
    /// `RawConfig` and resolved through the same format-agnostic `resolve_config`
    /// every file loader ends in. Connection behavior matches
    /// [`from_file`](Self::from_file).
    #[staticmethod]
    fn from_config(py: Python<'_>, config: &Bound<'_, PyAny>) -> PyResult<Py<Self>> {
        let raw: RawConfig =
            depythonize(config).map_err(|e| PyValueError::new_err(e.to_string()))?;
        let resolved = resolve_config(raw).map_err(|e| PyValueError::new_err(e.to_string()))?;
        Self::build(py, resolved)
    }

    /// The ids of every configured device.
    fn ids(&self) -> PyResult<Vec<String>> {
        Ok(self.registry()?.ids())
    }

    /// The ids of every configured device group.
    fn groups(&self) -> PyResult<Vec<String>> {
        Ok(self.registry()?.group_ids())
    }

    /// Close every SSH connection and shut the tokio runtime down, in that
    /// order, while the interpreter is still alive. The GIL is released for the
    /// duration so russh's worker threads can keep logging through pyo3-log as
    /// they wind down. Idempotent: a second call (or a call after a `with`
    /// block) is a no-op, and any method used afterwards raises `RuntimeError`.
    fn close(&mut self, py: Python<'_>) {
        let sis_keepalive = self.sis_keepalive.take();
        let registry = self.registry.take();
        let runtime = self.runtime.take();
        py.detach(move || match runtime {
            Some(runtime) => {
                // Abort the SIS keepalive tasks and drop the connections inside the
                // runtime context so russh's Drop impls can reach the
                // still-running session tasks, then give those tasks (and the
                // aborted SIS keepalives, which still hold a device handle each) a
                // bounded moment to close cleanly.
                {
                    let _enter = runtime.enter();
                    drop(sis_keepalive);
                    drop(registry);
                }
                runtime.shutdown_timeout(Duration::from_secs(5));
            }
            None => {
                drop(sis_keepalive);
                drop(registry);
            }
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

    /// Read a built-in field (e.g. `"firmware"`, `"ssh_port"`) from a device or
    /// group. Against a group, returns a `dict` mapping each member's id to its
    /// value; against a single device, returns the value itself.
    fn query(&self, py: Python<'_>, target: &str, name: &str) -> PyResult<Py<PyAny>> {
        let query = Query::from_str(name).map_err(|e| PyValueError::new_err(e.to_string()))?;
        self.execute(py, target, query.instruction())
    }

    /// Run a recorder command (e.g. `"start"`, `"stop"`, `"pause"`) on a device
    /// or group. Against a group every member receives the command at once, and
    /// the return is a `dict` of each member's reply; against a single device it
    /// is that one reply.
    fn command(&self, py: Python<'_>, target: &str, name: &str) -> PyResult<Py<PyAny>> {
        let command = Command::from_str(name).map_err(|e| PyValueError::new_err(e.to_string()))?;
        self.execute(py, target, command.instruction())
    }

    /// Write `value` into a metadata register (e.g. `"title"`) on a device or
    /// group. The device truncates the value at its own length limit. Against a
    /// group, returns a `dict` of each member's echoed reply.
    fn register(
        &self,
        py: Python<'_>,
        target: &str,
        name: &str,
        value: &str,
    ) -> PyResult<Py<PyAny>> {
        let register =
            Register::from_str(name).map_err(|e| PyValueError::new_err(e.to_string()))?;
        self.execute(py, target, register.instruction(value))
    }
}

impl Sismatic {
    /// Construct the tokio runtime, registry, and SIS keepalive around a resolved
    /// config (devices plus groups), then register the `atexit` teardown. The
    /// shared tail of every `from_*` constructor.
    fn build(py: Python<'_>, resolved: Resolved) -> PyResult<Py<Self>> {
        let runtime = Runtime::new().map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        let registry = Registry::build(resolved.devices, resolved.groups, Arc::new(RusshConnector));
        let sis_keepalive = SisKeepalive::spawn(runtime.handle(), registry.devices());
        let session = Py::new(
            py,
            Self {
                sis_keepalive: Some(sis_keepalive),
                registry: Some(registry),
                runtime: Some(runtime),
            },
        )?;
        register_atexit_close(py, &session)?;
        Ok(session)
    }

    /// Borrow the registry, or raise if the session has been closed.
    fn registry(&self) -> PyResult<&Registry> {
        self.registry.as_ref().ok_or_else(closed_err)
    }

    /// Borrow the runtime, or raise if the session has been closed.
    fn runtime(&self) -> PyResult<&Runtime> {
        self.runtime.as_ref().ok_or_else(closed_err)
    }

    /// Resolve `target` to a device or a group, run `instruction` to completion
    /// on the runtime, and turn the reply into a native Python object. A device
    /// yields its single decoded value; a group yields a `dict` mapping each
    /// member's id to its value. The GIL is released for the duration of the
    /// (blocking) device exchange.
    fn execute(
        &self,
        py: Python<'_>,
        target: &str,
        instruction: Instruction,
    ) -> PyResult<Py<PyAny>> {
        let registry = self.registry()?;
        let runtime = self.runtime()?;

        match registry
            .target(target)
            .ok_or_else(|| PyValueError::new_err(format!("unknown device or group '{target}'")))?
        {
            Target::Device(device) => {
                let value = py
                    .detach(|| runtime.block_on(device.run(&instruction)))
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
                value_into_py(py, value)
            }
            Target::Group(group) => {
                let results = py
                    .detach(|| runtime.block_on(group.run(&instruction)))
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
                let dict = PyDict::new(py);
                for (member, value) in results {
                    dict.set_item(member, value_into_py(py, value)?)?;
                }
                dict.into_py_any(py)
            }
        }
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
