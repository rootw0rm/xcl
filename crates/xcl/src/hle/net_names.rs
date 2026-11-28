//! "A name that resolves without a network": the policy shared by the Windows
//! `XCL_NETWORK=0` route (`hle::netoff`) and the Linux `ws2_32` HLE route
//! (`hle::ws2_linux`). One definition, so the two platforms cannot drift.

/// A name that resolves without a network: none (the local addresses), this machine's
/// own name, localhost – a console without a network still answers these (a retail game
/// asks for its own name and none at start).
pub(crate) fn local_name(name: &str) -> bool {
    if name.is_empty() || name.eq_ignore_ascii_case("localhost") || name.starts_with("127.") || name == "::1" {
        return true;
    }
    hostname().is_some_and(|h| h.eq_ignore_ascii_case(name))
}

/// This machine's node name, the answer `gethostname` gives, or `None` when the host
/// cannot provide one.
pub(crate) fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    if !host::read(&mut buf) {
        return None;
    }
    let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..n]);
    (!name.is_empty()).then(|| name.into_owned())
}

#[cfg(windows)]
mod host {
    #[link(name = "ws2_32", kind = "raw-dylib")]
    extern "system" {
        fn gethostname(name: *mut u8, len: i32) -> i32;
    }

    pub(super) fn read(buf: &mut [u8; 256]) -> bool {
        unsafe { gethostname(buf.as_mut_ptr(), buf.len() as i32) == 0 }
    }
}

#[cfg(not(windows))]
mod host {
    pub(super) fn read(buf: &mut [u8; 256]) -> bool {
        unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) == 0 }
    }
}
