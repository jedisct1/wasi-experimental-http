use anyhow::Error;
use bytes::Bytes;
use futures::executor::block_on;
use http::{HeaderMap, HeaderValue};
use reqwest::{Client, Method};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::str::FromStr;
use tokio::runtime::Handle;
use url::Url;
use wasmtime::*;

const MEMORY: &str = "memory";

pub type WasiHandle = u32;

struct Body {
    bytes: Vec<u8>,
    pos: usize,
}

struct Response {
    headers: HeaderMap,
    body: Body,
}

#[derive(Default)]
pub struct State {
    responses: HashMap<WasiHandle, Response>,
    current_handle: WasiHandle,
}

#[derive(Debug, thiserror::Error)]
enum HttpError {
    #[error("Invalid handle: [{0}]")]
    InvalidHandle(WasiHandle),
    #[error("Memory not found")]
    MemoryNotFound,
    #[error("Memory access error")]
    MemoryAccessError(#[from] wasmtime::MemoryAccessError),
    #[error("Buffer too small")]
    BufferTooSmall,
    #[error("Header not found")]
    HeaderNotFound,
    #[error("UTF-8 error")]
    UTF8Error(#[from] std::str::Utf8Error),
    #[error("Destination not allowed")]
    DestinationNotAllowed(String),
    #[error("Invalid method")]
    InvalidMethod,
    #[error("Invalid encoding")]
    InvalidEncoding,
    #[error("Invalid URL")]
    InvalidUrl,
    #[error("HTTP error")]
    RequestError(#[from] reqwest::Error),
    #[error("Runtime error")]
    RuntimeError,
    #[error("Too many sessions")]
    TooManySessions,
}

impl From<HttpError> for u32 {
    fn from(e: HttpError) -> u32 {
        match e {
            HttpError::InvalidHandle(_) => 1,
            HttpError::MemoryNotFound => 2,
            HttpError::MemoryAccessError(_) => 3,
            HttpError::BufferTooSmall => 4,
            HttpError::HeaderNotFound => 5,
            HttpError::UTF8Error(_) => 6,
            HttpError::DestinationNotAllowed(_) => 7,
            HttpError::InvalidMethod => 8,
            HttpError::InvalidEncoding => 9,
            HttpError::InvalidUrl => 10,
            HttpError::RequestError(_) => 11,
            HttpError::RuntimeError => 12,
            HttpError::TooManySessions => 13,
        }
    }
}

fn memory_get(caller: Caller<'_>) -> Result<Memory, HttpError> {
    if let Some(Extern::Memory(mem)) = caller.get_export(MEMORY) {
        Ok(mem)
    } else {
        Err(HttpError::MemoryNotFound)
    }
}

fn slice_from_memory(memory: &Memory, offset: u32, len: u32) -> Result<&[u8], HttpError> {
    let required_memory_size = offset.checked_add(len).ok_or(HttpError::BufferTooSmall)? as usize;
    if required_memory_size > memory.data_size() {
        return Err(HttpError::BufferTooSmall);
    }
    let slice =
        &unsafe { memory.data_unchecked() }[offset as usize..offset as usize + len as usize];
    Ok(slice)
}

fn string_from_memory(memory: &Memory, offset: u32, len: u32) -> Result<&str, HttpError> {
    let slice = slice_from_memory(memory, offset, len)?;
    Ok(std::str::from_utf8(slice)?)
}

struct HostCalls;

impl HostCalls {
    fn close(
        st: Rc<RefCell<State>>,
        _caller: Caller<'_>,
        handle: WasiHandle,
    ) -> Result<(), HttpError> {
        st.borrow_mut().responses.remove(&handle);
        Ok(())
    }

    fn body_read(
        st: Rc<RefCell<State>>,
        caller: Caller<'_>,
        handle: WasiHandle,
        buf_ptr: u32,
        buf_len: u32,
        buf_read_ptr: u32,
    ) -> Result<(), HttpError> {
        let mut st = st.borrow_mut();
        let mut body = &mut st
            .responses
            .get_mut(&handle)
            .ok_or(HttpError::InvalidHandle(handle))?
            .body;
        let memory = memory_get(caller)?;
        let available = std::cmp::min(buf_len as _, body.bytes.len() - body.pos);
        memory.write(buf_ptr as _, &body.bytes[body.pos..body.pos + available])?;
        body.pos += available;
        memory.write(buf_read_ptr as _, &(available as u32).to_le_bytes())?;
        Ok(())
    }

    fn header_get(
        st: Rc<RefCell<State>>,
        caller: Caller<'_>,
        handle: WasiHandle,
        name_ptr: u32,
        name_len: u32,
        value_ptr: u32,
        value_len: u32,
        value_written_ptr: u32,
    ) -> Result<(), HttpError> {
        let st = st.borrow();
        let headers = &st
            .responses
            .get(&handle)
            .ok_or(HttpError::InvalidHandle(handle))?
            .headers;
        let memory = memory_get(caller)?;
        let key = string_from_memory(&memory, name_ptr, name_len)?.to_ascii_lowercase();
        let value = headers.get(key).ok_or(HttpError::HeaderNotFound)?;
        if value.len() > value_len as _ {
            return Err(HttpError::BufferTooSmall);
        }
        memory.write(value_ptr as _, value.as_bytes())?;
        memory.write(value_written_ptr as _, &(value.len() as u32).to_le_bytes())?;
        Ok(())
    }

    fn req(
        st: Rc<RefCell<State>>,
        allowed_hosts: Option<&[String]>,
        caller: Caller<'_>,
        url_ptr: u32,
        url_len: u32,
        method_ptr: u32,
        method_len: u32,
        req_headers_ptr: u32,
        req_headers_len: u32,
        req_body_ptr: u32,
        req_body_len: u32,
        status_code_ptr: u32,
        res_handle_ptr: u32,
    ) -> Result<(), HttpError> {
        let span = tracing::trace_span!("req");
        let _enter = span.enter();
        let memory = memory_get(caller)?;
        let url = string_from_memory(&memory, url_ptr, url_len)?;
        let method = Method::from_str(string_from_memory(&memory, method_ptr, method_len)?)
            .map_err(|_| HttpError::InvalidMethod)?;
        let req_body = slice_from_memory(&memory, req_body_ptr, req_body_len)?;
        let headers = wasi_experimental_http::string_to_header_map(string_from_memory(
            &memory,
            req_headers_ptr,
            req_headers_len,
        )?)
        .map_err(|_| HttpError::InvalidEncoding)?;

        if !is_allowed(url, allowed_hosts)? {
            return Err(HttpError::DestinationNotAllowed(url.to_string()));
        }

        let (status, resp_headers, resp_body) = request(url, headers, method, req_body)?;
        tracing::debug!(
            status,
            ?resp_headers,
            body_len = resp_body.as_ref().len(),
            "got HTTP response, writing back to memory"
        );

        memory.write(status_code_ptr as _, &status.to_le_bytes())?;

        let response = Response {
            headers: resp_headers,
            body: Body {
                bytes: resp_body.to_vec(),
                pos: 0,
            },
        };
        let mut st = st.borrow_mut();
        let initial_handle = st.current_handle;
        while st.responses.get(&st.current_handle).is_some() {
            st.current_handle += 1;
            if st.current_handle == initial_handle {
                return Err(HttpError::TooManySessions);
            }
        }
        let handle = st.current_handle;
        st.responses.insert(handle, response);
        memory.write(res_handle_ptr as _, &handle.to_le_bytes())?;

        Ok(())
    }
}

pub struct Http {
    state: Rc<RefCell<State>>,
    allowed_hosts: Rc<Option<Vec<String>>>,
}

impl Http {
    pub const MODULE: &'static str = "wasi_experimental_http";

    pub fn new(allowed_hosts: Option<Vec<String>>) -> Result<Self, Error> {
        let state = Rc::new(RefCell::new(State::default()));
        let allowed_hosts = Rc::new(allowed_hosts);
        Ok(Http {
            state,
            allowed_hosts,
        })
    }

    pub fn add_to_linker(&self, linker: &mut Linker) -> Result<(), Error> {
        let st = self.state.clone();
        linker.func(
            Self::MODULE,
            "close",
            move |caller: Caller<'_>, handle: WasiHandle| -> u32 {
                match HostCalls::close(st.clone(), caller, handle) {
                    Ok(()) => 0,
                    Err(e) => e.into(),
                }
            },
        )?;

        let st = self.state.clone();
        linker.func(
            Self::MODULE,
            "body_read",
            move |caller: Caller<'_>,
                  handle: WasiHandle,
                  buf_ptr: u32,
                  buf_len: u32,
                  buf_read_ptr: u32|
                  -> u32 {
                match HostCalls::body_read(
                    st.clone(),
                    caller,
                    handle,
                    buf_ptr,
                    buf_len,
                    buf_read_ptr,
                ) {
                    Ok(()) => 0,
                    Err(e) => e.into(),
                }
            },
        )?;

        let st = self.state.clone();
        linker.func(
            Self::MODULE,
            "header_get",
            move |caller: Caller<'_>,
                  handle: WasiHandle,
                  name_ptr: u32,
                  name_len: u32,
                  value_ptr: u32,
                  value_len: u32,
                  value_written_ptr: u32|
                  -> u32 {
                match HostCalls::header_get(
                    st.clone(),
                    caller,
                    handle,
                    name_ptr,
                    name_len,
                    value_ptr,
                    value_len,
                    value_written_ptr,
                ) {
                    Ok(()) => 0,
                    Err(e) => e.into(),
                }
            },
        )?;

        let st = self.state.clone();
        let allowed_hosts = self.allowed_hosts.clone();
        linker.func(
            Self::MODULE,
            "req",
            move |caller: Caller<'_>,
                  url_ptr: u32,
                  url_len: u32,
                  method_ptr: u32,
                  method_len: u32,
                  req_headers_ptr: u32,
                  req_headers_len: u32,
                  req_body_ptr: u32,
                  req_body_len: u32,
                  status_code_ptr: u32,
                  res_handle_ptr: u32|
                  -> u32 {
                match HostCalls::req(
                    st.clone(),
                    allowed_hosts.as_deref(),
                    caller,
                    url_ptr,
                    url_len,
                    method_ptr,
                    method_len,
                    req_headers_ptr,
                    req_headers_len,
                    req_body_ptr,
                    req_body_len,
                    status_code_ptr,
                    res_handle_ptr,
                ) {
                    Ok(()) => 0,
                    Err(e) => e.into(),
                }
            },
        )?;

        Ok(())
    }
}

#[tracing::instrument]
fn request(
    url: &str,
    headers: HeaderMap,
    method: Method,
    body: &[u8],
) -> Result<(u16, HeaderMap<HeaderValue>, Bytes), HttpError> {
    tracing::debug!(
        %url,
        ?headers,
        ?method,
        body_len = body.len(),
        "performing request"
    );
    let url: Url = url.parse().map_err(|_| HttpError::InvalidUrl)?;
    let body = body.to_vec();
    match Handle::try_current() {
        Ok(r) => {
            // If running in a Tokio runtime, spawn a new blocking executor
            // that will send the HTTP request, and block on its execution.
            // This attempts to avoid any deadlocks from other operations
            // already executing on the same executor (compared with just
            // blocking on the current one).
            //
            // This should only be a temporary workaround, until we take
            // advantage of async functions in Wasmtime.
            tracing::trace!("tokio runtime available, spawning request on tokio thread");
            block_on(r.spawn_blocking(move || {
                let client = Client::builder().build().unwrap();
                let res = block_on(
                    client
                        .request(method, url)
                        .headers(headers)
                        .body(body)
                        .send(),
                )?;
                Ok((
                    res.status().as_u16(),
                    res.headers().clone(),
                    block_on(res.bytes())?,
                ))
            }))
            .map_err(|_| HttpError::RuntimeError)?
        }
        Err(_) => {
            tracing::trace!("no tokio runtime available, using blocking request");
            let res = reqwest::blocking::Client::new()
                .request(method, url)
                .headers(headers)
                .body(body)
                .send()?;
            return Ok((res.status().as_u16(), res.headers().clone(), res.bytes()?));
        }
    }
}

fn is_allowed(url: &str, allowed_domains: Option<&[String]>) -> Result<bool, HttpError> {
    let url_host = Url::parse(url)
        .map_err(|_| HttpError::InvalidUrl)?
        .host_str()
        .ok_or(HttpError::InvalidUrl)?
        .to_owned();
    match allowed_domains {
        Some(domains) => {
            let allowed: Result<Vec<_>, _> = domains.iter().map(|d| Url::parse(d)).collect();
            let allowed = allowed.map_err(|_| HttpError::InvalidUrl)?;
            let a: Vec<&str> = allowed.iter().map(|u| u.host_str().unwrap()).collect();
            Ok(a.contains(&url_host.as_str()))
        }
        None => Ok(false),
    }
}

#[test]
fn test_allowed_domains() {
    let allowed_domains = vec![
        "https://api.brigade.sh".to_string(),
        "https://example.com".to_string(),
        "http://192.168.0.1".to_string(),
    ];

    assert_eq!(
        true,
        is_allowed(
            "https://api.brigade.sh/healthz",
            Some(allowed_domains.as_ref())
        )
        .unwrap()
    );
    assert_eq!(
        true,
        is_allowed(
            "https://example.com/some/path/with/more/paths",
            Some(allowed_domains.as_ref())
        )
        .unwrap()
    );
    assert_eq!(
        true,
        is_allowed("http://192.168.0.1/login", Some(allowed_domains.as_ref())).unwrap()
    );
    assert_eq!(
        false,
        is_allowed("https://test.brigade.sh", Some(allowed_domains.as_ref())).unwrap()
    );
}
