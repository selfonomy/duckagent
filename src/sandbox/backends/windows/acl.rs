use crate::sandbox::backends::windows::winutil::to_wide_os;
use anyhow::{Result, anyhow};
use std::ffi::c_void;
use std::path::Path;
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW,
    TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{ACL, DACL_SECURITY_INFORMATION};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA,
};

const SE_FILE_OBJECT: i32 = 1;
const OBJECT_INHERIT_ACE: u32 = 0x1;
const CONTAINER_INHERIT_ACE: u32 = 0x2;
const SET_ACCESS: i32 = 2;
const DENY_ACCESS: i32 = 3;
const GENERIC_WRITE_MASK: u32 = 0x4000_0000;

pub const READ_MASK: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
pub const WRITE_MASK: u32 = FILE_GENERIC_READ
    | FILE_GENERIC_WRITE
    | FILE_GENERIC_EXECUTE
    | FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_WRITE_ATTRIBUTES
    | GENERIC_WRITE_MASK
    | DELETE
    | FILE_DELETE_CHILD;
pub const ALL_MASK: u32 = READ_MASK | WRITE_MASK;

pub unsafe fn ensure_allow_ace(path: &Path, sid: *mut c_void, mask: u32) -> Result<()> {
    unsafe { add_acl_entry(path, sid, mask, SET_ACCESS) }
}

pub unsafe fn ensure_deny_ace(path: &Path, sid: *mut c_void, mask: u32) -> Result<()> {
    unsafe { add_acl_entry(path, sid, mask, DENY_ACCESS) }
}

unsafe fn add_acl_entry(path: &Path, sid: *mut c_void, mask: u32, mode: i32) -> Result<()> {
    unsafe {
        let mut security_descriptor: *mut c_void = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let path_w = to_wide_os(path.as_os_str());
        let read_code = GetNamedSecurityInfoW(
            path_w.as_ptr() as *mut u16,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut security_descriptor,
        );
        if read_code != ERROR_SUCCESS {
            return Err(anyhow!(
                "GetNamedSecurityInfoW failed for {}: {}",
                path.display(),
                read_code
            ));
        }

        let trustee = TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid as *mut u16,
        };
        let explicit = EXPLICIT_ACCESS_W {
            grfAccessPermissions: mask,
            grfAccessMode: mode,
            grfInheritance: OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
            Trustee: trustee,
        };

        let mut new_dacl: *mut ACL = std::ptr::null_mut();
        let merge_code = SetEntriesInAclW(1, &explicit, dacl, &mut new_dacl);
        if merge_code == ERROR_SUCCESS {
            let write_code = SetNamedSecurityInfoW(
                path_w.as_ptr() as *mut u16,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                new_dacl,
                std::ptr::null_mut(),
            );
            if !new_dacl.is_null() {
                LocalFree(new_dacl as HLOCAL);
            }
            if !security_descriptor.is_null() {
                LocalFree(security_descriptor as HLOCAL);
            }
            if write_code != ERROR_SUCCESS {
                return Err(anyhow!(
                    "SetNamedSecurityInfoW failed for {}: {}",
                    path.display(),
                    write_code
                ));
            }
            return Ok(());
        }

        if !security_descriptor.is_null() {
            LocalFree(security_descriptor as HLOCAL);
        }
        Err(anyhow!(
            "SetEntriesInAclW failed for {}: {}",
            path.display(),
            merge_code
        ))
    }
}
