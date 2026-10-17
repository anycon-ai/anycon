//! pyo3 绑定骨架（feature `python`）：模块名 `anycon_crypto`，导出与 wasm 同名的类和函数。
//!
//! 只保证 `cargo check --features python` 能过；wheel 构建（maturin）、类型存根和测试在 v0.1 之后。
//! Python 侧的约定与 wasm 一致：密钥 / 消息是 `bytes`，失败抛 `ValueError`，消息就是 Rust 错误文本。

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use crate::{Handshake, Keypair, Role, Transport};

fn py_err(e: crate::Error) -> PyErr {
    PyValueError::new_err(e.to_string())
}

fn key32(bytes: &[u8]) -> PyResult<[u8; 32]> {
    <[u8; 32]>::try_from(bytes).map_err(|_| py_err(crate::Error::InvalidKey))
}

fn parse_role(role: &str) -> PyResult<Role> {
    match role {
        "initiator" => Ok(Role::Initiator),
        "responder" => Ok(Role::Responder),
        other => Err(PyValueError::new_err(format!("invalid role {other:?}, expected \"initiator\" or \"responder\""))),
    }
}

#[pyclass(name = "Keypair")]
pub struct PyKeypair {
    inner: Keypair,
}

#[pymethods]
impl PyKeypair {
    #[staticmethod]
    fn generate() -> Self {
        PyKeypair { inner: Keypair::generate() }
    }

    #[staticmethod]
    fn from_secret(secret: &[u8]) -> PyResult<Self> {
        Ok(PyKeypair { inner: Keypair::from_secret(key32(secret)?) })
    }

    #[getter]
    fn public<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.public)
    }

    #[getter]
    fn secret<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.secret)
    }
}

#[pyclass(name = "Handshake")]
pub struct PyHandshake {
    inner: Option<Handshake>,
}

impl PyHandshake {
    fn get(&self) -> PyResult<&Handshake> {
        self.inner.as_ref().ok_or_else(|| PyValueError::new_err("handshake already converted into a transport"))
    }
    fn get_mut(&mut self) -> PyResult<&mut Handshake> {
        self.inner.as_mut().ok_or_else(|| PyValueError::new_err("handshake already converted into a transport"))
    }
}

#[pymethods]
impl PyHandshake {
    #[staticmethod]
    fn pairing(role: &str, kp: &PyKeypair, psk: &[u8]) -> PyResult<Self> {
        let inner = Handshake::pairing(parse_role(role)?, &kp.inner, &key32(psk)?).map_err(py_err)?;
        Ok(PyHandshake { inner: Some(inner) })
    }

    #[staticmethod]
    #[pyo3(signature = (role, kp, remote_public=None))]
    fn reconnect(role: &str, kp: &PyKeypair, remote_public: Option<&[u8]>) -> PyResult<Self> {
        let remote = remote_public.map(key32).transpose()?;
        let inner = Handshake::reconnect(parse_role(role)?, &kp.inner, remote.as_ref()).map_err(py_err)?;
        Ok(PyHandshake { inner: Some(inner) })
    }

    fn is_my_turn(&self) -> PyResult<bool> {
        Ok(self.get()?.is_my_turn())
    }

    fn write<'py>(&mut self, py: Python<'py>, payload: &[u8]) -> PyResult<Bound<'py, PyBytes>> {
        let out = self.get_mut()?.write(payload).map_err(py_err)?;
        Ok(PyBytes::new(py, &out))
    }

    fn read<'py>(&mut self, py: Python<'py>, message: &[u8]) -> PyResult<Bound<'py, PyBytes>> {
        let out = self.get_mut()?.read(message).map_err(py_err)?;
        Ok(PyBytes::new(py, &out))
    }

    fn is_finished(&self) -> PyResult<bool> {
        Ok(self.get()?.is_finished())
    }

    fn remote_public<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyBytes>>> {
        Ok(self.get()?.remote_public().map(|k| PyBytes::new(py, &k)))
    }

    // Python 对象没法被「消费」，所以拿 &mut self 并把内部状态 take 走；名字和 Rust / wasm 保持一致。
    #[allow(clippy::wrong_self_convention)]
    fn into_transport(&mut self) -> PyResult<PyTransport> {
        let hs =
            self.inner.take().ok_or_else(|| PyValueError::new_err("handshake already converted into a transport"))?;
        Ok(PyTransport { inner: hs.into_transport().map_err(py_err)? })
    }
}

#[pyclass(name = "Transport")]
pub struct PyTransport {
    inner: Transport,
}

#[pymethods]
impl PyTransport {
    fn encrypt<'py>(&mut self, py: Python<'py>, plaintext: &[u8]) -> PyResult<Bound<'py, PyBytes>> {
        let out = self.inner.encrypt(plaintext).map_err(py_err)?;
        Ok(PyBytes::new(py, &out))
    }

    fn decrypt<'py>(&mut self, py: Python<'py>, ciphertext: &[u8]) -> PyResult<Bound<'py, PyBytes>> {
        let out = self.inner.decrypt(ciphertext).map_err(py_err)?;
        Ok(PyBytes::new(py, &out))
    }

    fn remote_public<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.remote_public())
    }
}

#[pyfunction]
fn derive_psk<'py>(py: Python<'py>, code: &str) -> PyResult<Bound<'py, PyBytes>> {
    let psk = crate::derive_psk(code).map_err(py_err)?;
    Ok(PyBytes::new(py, &psk))
}

#[pyfunction]
fn normalize_code(code: &str) -> String {
    crate::normalize_code(code)
}

#[pyfunction]
fn generate_pair_code() -> String {
    crate::generate_pair_code()
}

#[pyfunction]
fn fingerprint(public: &[u8]) -> PyResult<String> {
    Ok(crate::fingerprint(&key32(public)?))
}

// ---- 账号内接入（ADR-36）----

#[pyclass(name = "Cpace")]
pub struct PyCpace {
    inner: Option<crate::Cpace>,
}

#[pymethods]
impl PyCpace {
    #[new]
    fn new(prs: &[u8], ci: &[u8], sid: &[u8]) -> PyResult<Self> {
        Ok(PyCpace { inner: Some(crate::Cpace::new(prs, ci, sid).map_err(py_err)?) })
    }

    fn share<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let c = self.inner.as_ref().ok_or_else(|| PyValueError::new_err("cpace already finished"))?;
        Ok(PyBytes::new(py, &c.share()))
    }

    fn finish<'py>(&mut self, py: Python<'py>, role: &str, peer_share: &[u8]) -> PyResult<Bound<'py, PyBytes>> {
        let c = self.inner.take().ok_or_else(|| PyValueError::new_err("cpace already finished"))?;
        let psk = c.finish(parse_role(role)?, peer_share).map_err(py_err)?;
        Ok(PyBytes::new(py, &psk))
    }
}

#[pyfunction]
fn derive_enroll_password_prs<'py>(py: Python<'py>, password: &str, salt: &[u8]) -> PyResult<Bound<'py, PyBytes>> {
    let prs = crate::derive_enroll_password_prs(password, salt).map_err(py_err)?;
    Ok(PyBytes::new(py, &prs))
}

#[pyfunction]
fn derive_enroll_scan_psk<'py>(py: Python<'py>, secret: &[u8], node_id: &str) -> PyResult<Bound<'py, PyBytes>> {
    Ok(PyBytes::new(py, &crate::derive_enroll_scan_psk(&key32(secret)?, node_id)))
}

#[pyfunction]
fn enroll_channel_id<'py>(py: Python<'py>, node_id: &str, method: &str) -> Bound<'py, PyBytes> {
    PyBytes::new(py, &crate::enroll_channel_id(node_id, method))
}

#[pyfunction]
fn enroll_sid<'py>(py: Python<'py>, initiator_nonce: &[u8], responder_nonce: &[u8]) -> Bound<'py, PyBytes> {
    PyBytes::new(py, &crate::enroll_sid(initiator_nonce, responder_nonce))
}

#[pyfunction]
fn normalize_enroll_code(code: &str) -> PyResult<String> {
    crate::normalize_enroll_code(code).map_err(py_err)
}

#[pymodule]
fn anycon_crypto(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyKeypair>()?;
    m.add_class::<PyHandshake>()?;
    m.add_class::<PyTransport>()?;
    m.add_function(wrap_pyfunction!(derive_psk, m)?)?;
    m.add_function(wrap_pyfunction!(normalize_code, m)?)?;
    m.add_function(wrap_pyfunction!(generate_pair_code, m)?)?;
    m.add_function(wrap_pyfunction!(fingerprint, m)?)?;
    m.add_class::<PyCpace>()?;
    m.add_function(wrap_pyfunction!(derive_enroll_password_prs, m)?)?;
    m.add_function(wrap_pyfunction!(derive_enroll_scan_psk, m)?)?;
    m.add_function(wrap_pyfunction!(enroll_channel_id, m)?)?;
    m.add_function(wrap_pyfunction!(enroll_sid, m)?)?;
    m.add_function(wrap_pyfunction!(normalize_enroll_code, m)?)?;
    Ok(())
}
