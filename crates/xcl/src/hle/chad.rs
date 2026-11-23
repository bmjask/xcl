//! Silent lifecycle compatibility for the console DSP (`chad.dll`).
//!
//! Handles are opaque registry-issued integers. No audio is processed, but clients and
//! streams retain the ownership and lifetime rules expected by callers.

#![allow(non_snake_case)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{LazyLock, Mutex};

use crate::util::{E_INVALIDARG, E_OUTOFMEMORY, HRESULT, S_OK};

const XDSP_E_NOT_ALL_HANDLES_DEACTIVATED: HRESULT = 0x8AC8_10A4_u32 as i32;
const CLIENT_TAG: usize = 1;
const STREAM_TAG: usize = 2;

#[derive(Debug)]
struct Client {
    maximum_streams: u32,
    active_streams: u32,
}

#[derive(Debug)]
struct Registry {
    next_id: usize,
    clients: HashMap<usize, Client>,
    streams: HashMap<usize, usize>,
}

impl Registry {
    fn new() -> Self {
        Self { next_id: 1, clients: HashMap::new(), streams: HashMap::new() }
    }

    /// Tokens are never reused. Refuse allocation at exhaustion rather than making an old,
    /// stale token identify a newly-created object after wraparound.
    fn issue(&mut self, tag: usize) -> Option<usize> {
        let id = self.next_id;
        if id > (usize::MAX >> 2) {
            return None;
        }
        self.next_id += 1;
        Some((id << 2) | tag)
    }
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::new()));

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub unsafe extern "system" fn XDspConnectWithMaximumStreamLimit(
    _configuration: u64,
    _sample_rate: u32,
    maximum_streams: u32,
    _command_size: u32,
    _flags: u32,
    out_client: *mut usize,
) -> HRESULT {
    if out_client.is_null() {
        return E_INVALIDARG;
    }
    unsafe { *out_client = 0 };

    let mut registry = registry();
    let Some(client) = registry.issue(CLIENT_TAG) else { return E_OUTOFMEMORY };
    registry.clients.insert(client, Client { maximum_streams, active_streams: 0 });
    unsafe { *out_client = client };
    S_OK
}

pub unsafe extern "system" fn XDspActivate(
    client: usize,
    _impulse_response: *const c_void,
    _impulse_response_length: u32,
    out_stream: *mut usize,
) -> HRESULT {
    if out_stream.is_null() {
        return E_INVALIDARG;
    }
    unsafe { *out_stream = 0 };

    let mut registry = registry();
    let Some(owner) = registry.clients.get(&client) else { return E_INVALIDARG };
    if owner.active_streams >= owner.maximum_streams {
        return E_OUTOFMEMORY;
    }
    let Some(stream) = registry.issue(STREAM_TAG) else { return E_OUTOFMEMORY };
    registry.streams.insert(stream, client);
    registry.clients.get_mut(&client).expect("validated client disappeared").active_streams += 1;
    unsafe { *out_stream = stream };
    S_OK
}

pub unsafe extern "system" fn XDspSubmitCommand(stream: usize, _command: *const c_void) -> HRESULT {
    if registry().streams.contains_key(&stream) { S_OK } else { E_INVALIDARG }
}

pub unsafe extern "system" fn XDspDeactivate(stream: usize) -> HRESULT {
    let mut registry = registry();
    let Some(client) = registry.streams.remove(&stream) else { return E_INVALIDARG };
    let Some(owner) = registry.clients.get_mut(&client) else { return E_INVALIDARG };
    owner.active_streams -= 1;
    S_OK
}

pub unsafe extern "system" fn XDspDisconnect(client: usize) -> HRESULT {
    let mut registry = registry();
    let Some(owner) = registry.clients.get(&client) else { return E_INVALIDARG };
    if owner.active_streams != 0 {
        return XDSP_E_NOT_ALL_HANDLES_DEACTIVATED;
    }
    registry.clients.remove(&client);
    S_OK
}

pub fn exports() -> Vec<(&'static str, usize)> {
    crate::hle_exports![
        XDspActivate,
        XDspConnectWithMaximumStreamLimit,
        XDspDeactivate,
        XDspDisconnect,
        XDspSubmitCommand,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn reset() {
        *registry() = Registry::new();
    }

    #[test]
    fn exports_all_xdsp_entry_points() {
        let names: Vec<_> = exports().into_iter().map(|(name, _)| name).collect();
        assert_eq!(
            names,
            [
                "XDspActivate",
                "XDspConnectWithMaximumStreamLimit",
                "XDspDeactivate",
                "XDspDisconnect",
                "XDspSubmitCommand",
            ]
        );
    }

    #[test]
    fn validates_outputs_and_enforces_stream_lifetimes() {
        let _serial = TEST_LOCK.lock().unwrap();
        reset();
        unsafe {
            assert_eq!(XDspConnectWithMaximumStreamLimit(0, 0, 1, 0, 0, std::ptr::null_mut()), E_INVALIDARG);

            let mut client = 0;
            assert_eq!(XDspConnectWithMaximumStreamLimit(0, 0, 1, 0, 0, &mut client), S_OK);
            assert_ne!(client, 0);
            assert_eq!(XDspActivate(client, std::ptr::null(), 0, std::ptr::null_mut()), E_INVALIDARG);

            let mut stream = 0;
            assert_eq!(XDspActivate(client, std::ptr::null(), 0, &mut stream), S_OK);
            assert_ne!(stream, 0);
            assert_eq!(XDspSubmitCommand(stream, std::ptr::null()), S_OK);

            let mut over_limit = usize::MAX;
            assert_eq!(XDspActivate(client, std::ptr::null(), 0, &mut over_limit), E_OUTOFMEMORY);
            assert_eq!(over_limit, 0);
            assert_eq!(XDspDisconnect(client), XDSP_E_NOT_ALL_HANDLES_DEACTIVATED);

            assert_eq!(XDspDeactivate(stream), S_OK);
            assert_eq!(XDspDeactivate(stream), E_INVALIDARG);
            assert_eq!(XDspSubmitCommand(stream, std::ptr::null()), E_INVALIDARG);
            assert_eq!(XDspDisconnect(client), S_OK);
            assert_eq!(XDspDisconnect(client), E_INVALIDARG);
        }
    }

    #[test]
    fn clients_and_streams_receive_distinct_nonzero_tokens() {
        let _serial = TEST_LOCK.lock().unwrap();
        reset();
        unsafe {
            let (mut first, mut second) = (0, 0);
            assert_eq!(XDspConnectWithMaximumStreamLimit(0, 0, 2, 0, 0, &mut first), S_OK);
            assert_eq!(XDspConnectWithMaximumStreamLimit(0, 0, 1, 0, 0, &mut second), S_OK);
            assert_ne!(first, second);

            let (mut a, mut b) = (0, 0);
            assert_eq!(XDspActivate(first, std::ptr::null(), 0, &mut a), S_OK);
            assert_eq!(XDspActivate(first, std::ptr::null(), 0, &mut b), S_OK);
            assert_ne!(a, b);
            assert_ne!(a, first);
            assert_ne!(b, second);

            assert_eq!(XDspDeactivate(a), S_OK);
            assert_eq!(XDspDeactivate(b), S_OK);
            assert_eq!(XDspDisconnect(first), S_OK);
            assert_eq!(XDspDisconnect(second), S_OK);
        }
    }
}
