//! Windows file-permission hardening for the managed-agent store.
//!
//! `managed-agents.json` can hold plaintext agent private keys. The Unix
//! path sets 0o600; there was no Windows equivalent at all, so the file
//! inherited a directory ACL that let another group read it (BUG-014).
//!
//! Extracted from `storage.rs` to keep that file under the repo's
//! file-size ratchet; the logic is unchanged.

use std::path::Path;

#[cfg(windows)]
pub(super) fn restrict_file_to_current_user(path: &Path) -> Result<(), String> {
    use std::mem::size_of as size_of_ty;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        AddAccessAllowedAce, GetLengthSid, GetTokenInformation, InitializeAcl,
        InitializeSecurityDescriptor, SetFileSecurityW, SetSecurityDescriptorControl,
        SetSecurityDescriptorDacl, TokenUser, ACCESS_ALLOWED_ACE, ACL, ACL_REVISION,
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        SECURITY_DESCRIPTOR, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// Closes the process token on the way out of every early return below.
    struct TokenHandle(HANDLE);
    impl Drop for TokenHandle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    // ── The current user's SID, read off this process's access token ──────
    let mut raw_token: HANDLE = std::ptr::null_mut();
    // SAFETY: `raw_token` is a valid out-param slot; TOKEN_QUERY is the least
    // right `GetTokenInformation` needs. The pseudo-handle from
    // `GetCurrentProcess` needs no closing.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
        return Err(format!(
            "OpenProcessToken: {}",
            std::io::Error::last_os_error()
        ));
    }
    let token = TokenHandle(raw_token);

    // The sizing call is *expected* to fail with ERROR_INSUFFICIENT_BUFFER, so
    // only the returned length is consulted.
    let mut needed: u32 = 0;
    // SAFETY: a null buffer with length 0 is the documented sizing form.
    unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(format!(
            "GetTokenInformation(TokenUser) sizing: {}",
            std::io::Error::last_os_error()
        ));
    }
    // `u64` elements so the buffer is 8-byte aligned: TOKEN_USER holds a
    // pointer, and a `Vec<u8>` is only guaranteed 1-byte alignment.
    let mut user_buf = vec![0u64; (needed as usize).div_ceil(size_of_ty::<u64>())];
    // SAFETY: buffer is at least `needed` bytes and correctly aligned.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            user_buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(format!(
            "GetTokenInformation(TokenUser): {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: the call above filled `user_buf` with a TOKEN_USER. `sid` points
    // into `user_buf`, which outlives every use below.
    let sid = unsafe { (*user_buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    // SAFETY: `sid` came from the kernel and is well-formed.
    let sid_len = unsafe { GetLengthSid(sid) };
    if sid_len == 0 {
        return Err(format!("GetLengthSid: {}", std::io::Error::last_os_error()));
    }

    // ── A one-entry ACL: full control for that SID, nobody else ───────────
    // `ACCESS_ALLOWED_ACE::SidStart` is the first DWORD of the variable-length
    // SID that trails the struct, hence subtracting it before adding the real
    // SID length.
    let acl_len = size_of_ty::<ACL>() + size_of_ty::<ACCESS_ALLOWED_ACE>() - size_of_ty::<u32>()
        + sid_len as usize;
    let mut acl_buf = vec![0u64; acl_len.div_ceil(size_of_ty::<u64>())];
    let acl_bytes = (acl_buf.len() * size_of_ty::<u64>()) as u32;
    let acl = acl_buf.as_mut_ptr().cast::<ACL>();
    // SAFETY: `acl` points at `acl_bytes` of zeroed, 8-byte-aligned storage.
    if unsafe { InitializeAcl(acl, acl_bytes, ACL_REVISION) } == 0 {
        return Err(format!("InitializeAcl: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: the ACL was sized above to hold exactly this one ACE.
    if unsafe { AddAccessAllowedAce(acl, ACL_REVISION, FILE_ALL_ACCESS, sid) } == 0 {
        return Err(format!(
            "AddAccessAllowedAce: {}",
            std::io::Error::last_os_error()
        ));
    }

    // ── An absolute security descriptor carrying that ACL, protected ──────
    let mut descriptor = SECURITY_DESCRIPTOR::default();
    let descriptor_ptr: PSECURITY_DESCRIPTOR =
        (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast();
    // SAFETY: `descriptor` is a full SECURITY_DESCRIPTOR. `1` is
    // SECURITY_DESCRIPTOR_REVISION, which windows-sys does not re-export.
    if unsafe { InitializeSecurityDescriptor(descriptor_ptr, 1) } == 0 {
        return Err(format!(
            "InitializeSecurityDescriptor: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `acl` lives in `acl_buf`, which outlives the SetFileSecurityW
    // calls below. Args: DACL present = TRUE, DACL defaulted = FALSE.
    if unsafe { SetSecurityDescriptorDacl(descriptor_ptr, 1, acl, 0) } == 0 {
        return Err(format!(
            "SetSecurityDescriptorDacl: {}",
            std::io::Error::last_os_error()
        ));
    }
    // Without SE_DACL_PROTECTED the parent directory's inheritable ACEs are
    // merged straight back in and the single-entry ACL above buys nothing.
    // SAFETY: SE_DACL_PROTECTED is one of the inheritance control bits this
    // call is documented to accept.
    if unsafe { SetSecurityDescriptorControl(descriptor_ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED) }
        == 0
    {
        return Err(format!(
            "SetSecurityDescriptorControl(SE_DACL_PROTECTED): {}",
            std::io::Error::last_os_error()
        ));
    }

    // ── Apply it ──────────────────────────────────────────────────────────
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut last_error = None;
    for information in [
        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
        // Retry: the legacy entry point may reject the PROTECTED_* bit. The
        // SE_DACL_PROTECTED control bit already carries the same intent.
        DACL_SECURITY_INFORMATION,
    ] {
        // SAFETY: `wide` is NUL-terminated UTF-16; `descriptor_ptr` is a valid
        // absolute descriptor with a DACL that outlives this call.
        if unsafe { SetFileSecurityW(wide.as_ptr(), information, descriptor_ptr) } != 0 {
            return Ok(());
        }
        last_error = Some(std::io::Error::last_os_error());
    }
    Err(format!(
        "SetFileSecurityW {}: {}",
        path.display(),
        last_error.expect("loop ran at least once")
    ))
}

#[cfg(test)]
mod restricted_write_tests {
    use super::super::atomic_write_json_restricted;

    /// The write itself must land on every platform â€” the permission tightening
    /// is never allowed to cost the user their agent list.
    #[test]
    fn restricted_write_commits_the_payload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("managed-agents.json");

        atomic_write_json_restricted(&path, br#"{"agents":[]}"#).expect("restricted write");

        assert_eq!(
            std::fs::read(&path).expect("read back").as_slice(),
            br#"{"agents":[]}"#.as_slice()
        );
    }

    /// The committed file must carry an explicit, inheritance-blocked DACL.
    ///
    /// This is the regression test for the defect where the Windows branch was
    /// simply missing: the file inherited the parent directory's ACL, and on a
    /// real install a non-owner group held Modify on 17 plaintext private keys.
    /// Asserting `SE_DACL_PROTECTED` plus a single non-inherited ACE is exactly
    /// the shape that cannot happen by inheritance.
    #[cfg(windows)]
    #[test]
    fn restricted_write_blocks_inherited_aces_on_windows() {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Security::{
            GetAce, GetFileSecurityW, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
            ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, INHERITED_ACE, SE_DACL_PROTECTED,
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("managed-agents.json");
        atomic_write_json_restricted(&path, br#"{"agents":[]}"#).expect("restricted write");

        let resolved = std::fs::canonicalize(&path).unwrap_or(path);
        let wide: Vec<u16> = resolved
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // Read the committed DACL back out of the filesystem.
        let mut needed: u32 = 0;
        // SAFETY: null buffer with length 0 is the documented sizing form.
        unsafe {
            GetFileSecurityW(
                wide.as_ptr(),
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        assert!(
            needed > 0,
            "GetFileSecurityW sizing: {}",
            std::io::Error::last_os_error()
        );
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        let descriptor = buf.as_mut_ptr().cast();
        // SAFETY: buffer is at least `needed` bytes and 8-byte aligned.
        assert_ne!(
            unsafe {
                GetFileSecurityW(
                    wide.as_ptr(),
                    DACL_SECURITY_INFORMATION,
                    descriptor,
                    needed,
                    &mut needed,
                )
            },
            0,
            "GetFileSecurityW: {}",
            std::io::Error::last_os_error()
        );

        let mut control: u16 = 0;
        let mut revision: u32 = 0;
        // SAFETY: `descriptor` is a valid self-relative descriptor.
        assert_ne!(
            unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
            0,
            "GetSecurityDescriptorControl: {}",
            std::io::Error::last_os_error()
        );
        assert_ne!(
            control & SE_DACL_PROTECTED,
            0,
            "DACL is not protected: the parent directory's ACEs still apply"
        );

        let mut present: i32 = 0;
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut defaulted: i32 = 0;
        // SAFETY: all three out-params are valid slots.
        assert_ne!(
            unsafe {
                GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
            },
            0,
            "GetSecurityDescriptorDacl: {}",
            std::io::Error::last_os_error()
        );
        assert_ne!(present, 0, "no DACL on the written file");
        // A NULL DACL is not "no access" â€” it grants everyone full control.
        assert!(!dacl.is_null(), "NULL DACL grants everyone full control");

        // SAFETY: `dacl` points into `buf`, which is still alive.
        let ace_count = unsafe { (*dacl).AceCount };
        assert_eq!(
            ace_count, 1,
            "expected exactly one owner ACE, found {ace_count}"
        );

        let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: index 0 exists, per the AceCount assertion above.
        assert_ne!(
            unsafe { GetAce(dacl, 0, &mut ace) },
            0,
            "GetAce: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: the sole ACE was added by us as an ACCESS_ALLOWED_ACE.
        let flags = unsafe { (*ace.cast::<ACCESS_ALLOWED_ACE>()).Header.AceFlags } as u32;
        assert_eq!(
            flags & INHERITED_ACE,
            0,
            "the sole ACE is inherited from the parent directory"
        );
    }
}
