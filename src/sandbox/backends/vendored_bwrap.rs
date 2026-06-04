#![cfg(target_os = "linux")]

#[cfg(duckagent_vendored_bwrap)]
mod imp {
    use std::ffi::CString;
    use std::os::raw::c_char;

    unsafe extern "C" {
        fn duckagent_bwrap_main(argc: libc::c_int, argv: *const *const c_char) -> libc::c_int;
    }

    pub fn exec(argv: Vec<String>) -> ! {
        let cstrings = argv
            .iter()
            .map(|arg| CString::new(arg.as_str()))
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|error| panic!("failed to convert bwrap argv to CString: {error}"));
        let mut argv_ptrs = cstrings.iter().map(|arg| arg.as_ptr()).collect::<Vec<_>>();
        argv_ptrs.push(std::ptr::null());

        // SAFETY: argv_ptrs is null-terminated and points at CString storage
        // that remains alive for the duration of the call.
        let exit_code =
            unsafe { duckagent_bwrap_main(cstrings.len() as libc::c_int, argv_ptrs.as_ptr()) };
        std::process::exit(exit_code);
    }

    pub fn available() -> bool {
        true
    }
}

#[cfg(not(duckagent_vendored_bwrap))]
mod imp {
    pub fn exec(_argv: Vec<String>) -> ! {
        eprintln!(
            "duckagent vendored bubblewrap is not available in this build; rebuild on Linux with libcap development headers or install system bwrap"
        );
        std::process::exit(1);
    }

    pub fn available() -> bool {
        false
    }
}

pub fn exec(argv: Vec<String>) -> ! {
    imp::exec(argv)
}

pub fn available() -> bool {
    imp::available()
}
