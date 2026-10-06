//! A minimal blocking HTTPS `GET` on WinHTTP: the system's TLS stack, certificate store and proxy
//! settings, with no HTTP crate. Call it from a worker thread only.
//!
//! Deliberately small: no cookies, no authentication, no request bodies, a hard size limit, and a
//! deadline for the whole transfer (a server that trickles bytes cannot hold the thread forever).

use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetTimeouts,
};
use windows::core::{PCWSTR, w};

use super::util::wide;

/// Whole-transfer limit.
const DEADLINE: Duration = Duration::from_secs(45);

/// An open WinHTTP handle, closed on drop.
struct Handle(*mut c_void);

impl Handle {
    fn new(raw: *mut c_void, what: &str) -> Result<Handle, String> {
        if raw.is_null() {
            Err(format!(
                "{what} failed ({})",
                windows::core::Error::from_thread()
            ))
        } else {
            Ok(Handle(raw))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

/// Short, user-readable description of a transport failure.
fn describe(e: &windows::core::Error) -> String {
    // WinHTTP errors are 0x80190000-ish HRESULTs of the form 12xxx; the message text is localized
    // and long, so keep only the first sentence.
    let msg = e.message();
    let msg = msg.trim();
    let first = msg.split(['\r', '\n']).next().unwrap_or("");
    if first.is_empty() {
        format!("error {:#x}", e.code().0)
    } else {
        first.to_string()
    }
}

/// `GET https://host:port/path`, at most `max_bytes` of body. Errors are short descriptions.
pub fn get_https(host: &str, port: u16, path: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
    let started = Instant::now();
    unsafe {
        let agent = wide("SharkNotch/0.1");
        let session = Handle::new(
            WinHttpOpen(
                PCWSTR(agent.as_ptr()),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            ),
            "opening a session",
        )?;
        // resolve, connect, send, receive (milliseconds)
        let _ = WinHttpSetTimeouts(session.0, 8_000, 8_000, 12_000, 15_000);
        let host_w = wide(host);
        let conn = Handle::new(
            WinHttpConnect(session.0, PCWSTR(host_w.as_ptr()), port, 0),
            "connecting",
        )?;
        let path_w = wide(path);
        let req = Handle::new(
            WinHttpOpenRequest(
                conn.0,
                w!("GET"),
                PCWSTR(path_w.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            ),
            "creating the request",
        )?;
        let headers: Vec<u16> = "Accept: text/calendar, text/plain;q=0.5, */*;q=0.1\r\n"
            .encode_utf16()
            .collect();
        WinHttpSendRequest(req.0, Some(&headers), None, 0, 0, 0).map_err(|e| describe(&e))?;
        WinHttpReceiveResponse(req.0, std::ptr::null_mut()).map_err(|e| describe(&e))?;

        let mut status: u32 = 0;
        let mut len = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut c_void),
            &mut len,
            std::ptr::null_mut(),
        )
        .map_err(|e| describe(&e))?;
        if status != 200 {
            return Err(format!("HTTP {status}"));
        }

        let mut body: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; 32 * 1024];
        loop {
            if started.elapsed() > DEADLINE {
                return Err("timed out".into());
            }
            let mut avail = 0u32;
            WinHttpQueryDataAvailable(req.0, &mut avail).map_err(|e| describe(&e))?;
            if avail == 0 {
                break;
            }
            let want = (avail as usize).min(chunk.len());
            let mut read = 0u32;
            WinHttpReadData(
                req.0,
                chunk.as_mut_ptr() as *mut c_void,
                want as u32,
                &mut read,
            )
            .map_err(|e| describe(&e))?;
            if read == 0 {
                break;
            }
            if body.len() + read as usize > max_bytes {
                return Err(format!("larger than {} MB", max_bytes >> 20));
            }
            body.extend_from_slice(&chunk[..read as usize]);
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unresolvable_host_is_a_short_error_not_a_hang_or_a_panic() {
        // `.invalid` never resolves (RFC 2606); on a machine without network the failure is the
        // same kind. The point: a clean `Err`, quickly.
        let t = Instant::now();
        let r = get_https("shark-notch-test.invalid", 443, "/x.ics", 1 << 20);
        assert!(r.is_err());
        let msg = r.unwrap_err();
        assert!(!msg.is_empty() && msg.len() < 200, "{msg}");
        assert!(t.elapsed() < Duration::from_secs(30));
    }
}
