//! Probe (never merged): what the runner's glibc glob(3) lists for trailing `/`, `/.`.
#![cfg(all(target_os = "linux", target_env = "gnu"))]

use std::ffi::{CStr, CString, c_char, c_int, c_void};

#[repr(C)]
struct GlobT {
    gl_pathc: usize,
    gl_pathv: *mut *mut c_char,
    gl_offs: usize,
    gl_flags: c_int,
    rest: [*mut c_void; 5],
}

unsafe extern "C" {
    fn glob(
        pattern: *const c_char,
        flags: c_int,
        errfunc: Option<unsafe extern "C" fn(*const c_char, c_int) -> c_int>,
        pglob: *mut GlobT,
    ) -> c_int;
    fn globfree(pglob: *mut GlobT);
    fn gnu_get_libc_version() -> *const c_char;
}

fn list(pattern: &str) -> (c_int, Vec<String>) {
    let p = CString::new(pattern).expect("no NUL");
    let mut g = GlobT {
        gl_pathc: 0,
        gl_pathv: std::ptr::null_mut(),
        gl_offs: 0,
        gl_flags: 0,
        rest: [std::ptr::null_mut(); 5],
    };
    // SAFETY: probe only; g is a zeroed glob_t, freed below.
    let r = unsafe { glob(p.as_ptr(), 0, None, &mut g) };
    let mut out = Vec::new();
    for i in 0..g.gl_pathc {
        // SAFETY: glob filled gl_pathc entries.
        let s = unsafe { CStr::from_ptr(*g.gl_pathv.add(i)) };
        out.push(s.to_string_lossy().into_owned());
    }
    // SAFETY: g came from glob.
    unsafe { globfree(&mut g) };
    (r, out)
}

#[test]
fn probe() {
    let root = std::env::temp_dir().join(format!("kbfglobprobe{}", std::process::id()));
    std::fs::create_dir_all(root.join("etc/kbf/d")).expect("dirs");
    std::fs::write(root.join("etc/kbf/s3.env"), "A=1\n").expect("file");
    let r = root.to_string_lossy().into_owned();
    // SAFETY: static string from glibc.
    let v = unsafe { CStr::from_ptr(gnu_get_libc_version()) };
    let mut report = format!("glibc {}\n", v.to_string_lossy());
    for tail in [
        "/etc/kbf/s3.env",
        "/etc/kbf/s3.env/",
        "/etc/kbf/s3.env//",
        "/etc/kbf/s3.env/.",
        "/etc/kbf/s3.env/./",
        "/etc/kbf/s3.env/..",
        "/etc/kbf/s3*/",
        "/etc/kbf/s3.env*/",
        "/etc/*/s3.env/",
        "/etc/kbf/\\s3.env/",
        "/etc/kbf/s3.env\\/",
        "/etc/kbf/s3.env\\\\/",
        "/etc/kbf/*/",
        "/etc/kbf/d/",
        "/etc/kbf/.",
    ] {
        let (rc, got) = list(&format!("{r}{tail}"));
        let got: Vec<String> = got.iter().map(|g| g.replacen(&r, "", 1)).collect();
        report.push_str(&format!("{tail:24} rc={rc} {got:?}\n"));
    }
    panic!("PROBE\n{report}");
}
