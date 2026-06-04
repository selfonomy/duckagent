use crate::sandbox::backends::windows::winutil::to_wide_str;
use anyhow::{Result, bail};
use std::ffi::c_void;
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, GetLastError, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{LookupAccountNameW, SID_NAME_USE};

pub struct LocalSid {
    psid: *mut c_void,
}

impl LocalSid {
    pub fn from_string(value: &str) -> Result<Self> {
        let psid = convert_string_sid_to_sid(value)
            .ok_or_else(|| anyhow::anyhow!("invalid SID string `{value}`"))?;
        Ok(Self { psid })
    }

    pub fn as_ptr(&self) -> *mut c_void {
        self.psid
    }
}

impl Drop for LocalSid {
    fn drop(&mut self) {
        if !self.psid.is_null() {
            unsafe {
                LocalFree(self.psid as HLOCAL);
            }
        }
    }
}

pub fn convert_string_sid_to_sid(value: &str) -> Option<*mut c_void> {
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn ConvertStringSidToSidW(string_sid: *const u16, sid: *mut *mut c_void) -> i32;
    }

    let mut sid: *mut c_void = std::ptr::null_mut();
    let ok = unsafe { ConvertStringSidToSidW(to_wide_str(value).as_ptr(), &mut sid) };
    (ok != 0 && !sid.is_null()).then_some(sid)
}

pub fn local_sid(value: &str) -> Result<LocalSid> {
    if value.trim().is_empty() {
        bail!("cannot build SID from an empty string");
    }
    LocalSid::from_string(value)
}

pub fn account_sid_string(name: &str) -> Result<String> {
    let name_w = to_wide_str(name);
    let mut sid_len = 0u32;
    let mut domain_len = 0u32;
    let mut use_type: SID_NAME_USE = 0;
    unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            name_w.as_ptr(),
            std::ptr::null_mut(),
            &mut sid_len,
            std::ptr::null_mut(),
            &mut domain_len,
            &mut use_type,
        );
        let err = GetLastError();
        if err != ERROR_INSUFFICIENT_BUFFER {
            return Err(anyhow::anyhow!(
                "LookupAccountNameW preflight failed for `{name}`: {err}"
            ));
        }
        let mut sid = vec![0u8; sid_len as usize];
        let mut domain = vec![0u16; domain_len as usize];
        let ok = LookupAccountNameW(
            std::ptr::null(),
            name_w.as_ptr(),
            sid.as_mut_ptr() as *mut c_void,
            &mut sid_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut use_type,
        );
        if ok == 0 {
            return Err(anyhow::anyhow!(
                "LookupAccountNameW failed for `{name}`: {}",
                GetLastError()
            ));
        }
        let mut sid_string: *mut u16 = std::ptr::null_mut();
        let ok = ConvertSidToStringSidW(sid.as_mut_ptr() as *mut c_void, &mut sid_string);
        if ok == 0 || sid_string.is_null() {
            return Err(anyhow::anyhow!(
                "ConvertSidToStringSidW failed for `{name}`: {}",
                GetLastError()
            ));
        }
        let mut len = 0usize;
        while *sid_string.add(len) != 0 {
            len += 1;
        }
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(sid_string, len));
        LocalFree(sid_string as HLOCAL);
        Ok(out)
    }
}
