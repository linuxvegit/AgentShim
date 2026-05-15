//! Administrator-privilege check.
//!
//! `install`, `uninstall`, `start`, `stop`, `restart` operate on SCM,
//! which requires an elevated token. `status` only needs `SC_MANAGER_CONNECT`
//! + `SERVICE_QUERY_STATUS` and skips this check.
//!
//! We use `GetTokenInformation(token, TokenElevation, ...)` via the
//! `windows` crate. The check is cheap and idempotent.

use anyhow::{Context, Result};
use std::mem;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// RAII guard that calls `CloseHandle` on drop. Wraps a process token
/// or any other kernel HANDLE that must be released.
struct ScopedHandle(HANDLE);

impl Drop for ScopedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: `self.0` was returned by a Windows API that hands
            // out kernel handles requiring CloseHandle (per Win32 docs).
            // We hold the only reference and run on drop, so it can't
            // race with another close. Errors are ignored — there is no
            // meaningful recovery for a failed CloseHandle.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Returns `true` if the current process token is elevated.
pub fn is_elevated() -> Result<bool> {
    let mut raw_token = HANDLE::default();
    // SAFETY: `GetCurrentProcess` is a no-op pseudo-handle (always valid).
    // `OpenProcessToken` writes a new owned HANDLE into `raw_token` on
    // success; we check the Result and only proceed on Ok. The handle is
    // immediately wrapped in a `ScopedHandle` so it is released on every
    // function exit path (success or error).
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token)
            .context("OpenProcessToken failed")?;
    }
    let token = ScopedHandle(raw_token);

    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned_len = 0u32;
    // SAFETY: `token.0` is a valid process token obtained above. The
    // `tokeninformation` pointer points to a stack-allocated
    // `TOKEN_ELEVATION`, and we pass the correct size. The call only
    // writes into the buffer up to `tokeninformationlength` bytes.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned_len,
        )
        .context("GetTokenInformation failed")?;
    }

    Ok(elevation.TokenIsElevated != 0)
}

/// Bail with a clear error if the current process is not elevated.
pub fn require_admin() -> Result<()> {
    if !is_elevated()? {
        anyhow::bail!(
            "This command requires administrator privileges.\n\
             Please re-run from an elevated PowerShell or CMD."
        );
    }
    Ok(())
}
