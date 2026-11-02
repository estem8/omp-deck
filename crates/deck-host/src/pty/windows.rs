//! Windows ConPTY: `conpty.dll` sideloaded from the executable's directory when present (it
//! starts the bundled `OpenConsole.exe`), else the inbox kernel32 implementation. The child is
//! created suspended with the pseudoconsole attribute, assigned to a kill-on-close job object
//! (so its whole tree can be terminated and nothing escapes), then resumed.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{COORD, HPCON};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleHandleW, GetProcAddress, LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LoadLibraryExW,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, EXTENDED_STARTUPINFO_PRESENT,
    GetExitCodeProcess, INFINITE, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
    ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, WaitForSingleObject,
};

use super::{PtySize, Spawned, winargs};
use crate::error::{Error, Result};
use crate::protocol::SessionSpec;
use crate::winproc::{AttributeList, check, owned};

/// `PSEUDOCONSOLE_RESIZE_QUIRK` (not in windows-sys): avoids a full repaint on resize.
const PSEUDOCONSOLE_RESIZE_QUIRK: u32 = 0x2;
/// `PSEUDOCONSOLE_WIN32_INPUT_MODE` (not in windows-sys): ConPTY requests win32-input-mode
/// (`CSI ? 9001 h`) so keys keep their modifiers.
const PSEUDOCONSOLE_WIN32_INPUT_MODE: u32 = 0x4;
/// `CreatePseudoConsole` flags. `PSEUDOCONSOLE_PASSTHROUGH_MODE` (0x8) stays off until CI byte
/// probes prove it helps (only stable with the sideloaded DLL on build ≥ 22621).
const CONPTY_FLAGS: u32 = PSEUDOCONSOLE_RESIZE_QUIRK | PSEUDOCONSOLE_WIN32_INPUT_MODE;
/// Environment override of [`CONPTY_FLAGS`] for byte probes (`0x…` or decimal).
const CONPTY_FLAGS_ENV: &str = "OMP_DECK_CONPTY_FLAGS";

type CreateFn = unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> i32;
type ResizeFn = unsafe extern "system" fn(HPCON, COORD) -> i32;
type CloseFn = unsafe extern "system" fn(HPCON);

/// The ConPTY entry points in use.
struct ConPty {
    create: CreateFn,
    resize: ResizeFn,
    close: CloseFn,
}

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

impl ConPty {
    fn load(module: windows_sys::Win32::Foundation::HMODULE) -> Option<ConPty> {
        if module.is_null() {
            return None;
        }
        // SAFETY: valid module handle and NUL-terminated names; the transmutes turn the
        // FARPROCs into the documented signatures of these exports.
        unsafe {
            let create = GetProcAddress(module, c"CreatePseudoConsole".as_ptr().cast())?;
            let resize = GetProcAddress(module, c"ResizePseudoConsole".as_ptr().cast())?;
            let close = GetProcAddress(module, c"ClosePseudoConsole".as_ptr().cast())?;
            Some(ConPty {
                create: std::mem::transmute::<unsafe extern "system" fn() -> isize, CreateFn>(
                    create,
                ),
                resize: std::mem::transmute::<unsafe extern "system" fn() -> isize, ResizeFn>(
                    resize,
                ),
                close: std::mem::transmute::<unsafe extern "system" fn() -> isize, CloseFn>(close),
            })
        }
    }

    /// Sideloaded `conpty.dll` (application directory only) else kernel32.
    fn get() -> Result<&'static ConPty> {
        static API: OnceLock<Option<ConPty>> = OnceLock::new();
        API.get_or_init(|| {
            let dll = wide("conpty.dll".as_ref());
            // SAFETY: NUL-terminated name; the module stays loaded for the process lifetime.
            let sideloaded = unsafe {
                LoadLibraryExW(
                    dll.as_ptr(),
                    null_mut(),
                    LOAD_LIBRARY_SEARCH_APPLICATION_DIR,
                )
            };
            if let Some(api) = ConPty::load(sideloaded) {
                log::info!("using sideloaded conpty.dll");
                return Some(api);
            }
            let kernel32 = wide("kernel32.dll".as_ref());
            // SAFETY: NUL-terminated name; kernel32 is always loaded.
            ConPty::load(unsafe { GetModuleHandleW(kernel32.as_ptr()) })
        })
        .as_ref()
        .ok_or_else(|| Error::Os {
            call: "CreatePseudoConsole",
            source: io::Error::new(
                io::ErrorKind::Unsupported,
                "ConPTY needs Windows 10 1809 or later",
            ),
        })
    }
}

fn flags() -> u32 {
    std::env::var(CONPTY_FLAGS_ENV)
        .ok()
        .and_then(|v| match v.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => v.parse().ok(),
        })
        .unwrap_or(CONPTY_FLAGS)
}

fn coord(size: PtySize) -> COORD {
    COORD {
        X: size.cols.min(i16::MAX as u16) as i16,
        Y: size.rows.min(i16::MAX as u16) as i16,
    }
}

fn hresult(hr: i32, call: &'static str) -> Result<()> {
    if hr < 0 {
        Err(Error::Os {
            call,
            source: io::Error::from_raw_os_error(hr),
        })
    } else {
        Ok(())
    }
}

fn pipe() -> Result<(OwnedHandle, OwnedHandle)> {
    let (mut read, mut write) = (null_mut(), null_mut());
    // SAFETY: valid out pointers; default security, not inheritable.
    check(
        unsafe { CreatePipe(&mut read, &mut write, null(), 0) },
        "CreatePipe",
    )?;
    Ok((owned(read, "CreatePipe")?, owned(write, "CreatePipe")?))
}

/// An open pseudoconsole handle (closed exactly once).
struct Console(HPCON);

// SAFETY: HPCON is an opaque kernel object reference usable from any thread.
unsafe impl Send for Console {}

/// The child's environment block: inherited variables minus `remove`, plus `set`
/// (names case-insensitive, sorted as Windows expects), `K=V\0…\0\0` in UTF-16.
fn environment(spec: &SessionSpec) -> (Vec<u16>, Option<String>, Option<String>) {
    let mut vars: BTreeMap<String, (OsString, OsString)> = BTreeMap::new();
    for (k, v) in std::env::vars_os() {
        vars.insert(k.to_string_lossy().to_uppercase(), (k, v));
    }
    for k in &spec.env_remove {
        vars.remove(&k.to_uppercase());
    }
    for (k, v) in &spec.env_set {
        vars.insert(k.to_uppercase(), (k.into(), v.into()));
    }
    let lookup = |name: &str| {
        vars.get(name)
            .map(|(_, v)| v.to_string_lossy().into_owned())
    };
    let (path, pathext) = (lookup("PATH"), lookup("PATHEXT"));
    let mut block = Vec::new();
    for (k, v) in vars.values() {
        block.extend(k.encode_wide());
        block.push(u16::from(b'='));
        block.extend(v.encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    (block, path, pathext)
}

fn comspec() -> String {
    std::env::var("COMSPEC").unwrap_or_else(|_| {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
        format!(r"{root}\System32\cmd.exe")
    })
}

pub(crate) fn spawn(spec: &SessionSpec, size: PtySize) -> Result<Spawned> {
    let api = ConPty::get()?;
    let (env, path, pathext) = environment(spec);
    let program = winargs::resolve_program(&spec.program, path.as_deref(), pathext.as_deref());
    let line = winargs::command_line(&program, &spec.args, &comspec())?;
    let mut line: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
    let cwd = wide(spec.cwd.as_os_str());

    let (in_read, in_write) = pipe()?;
    let (out_read, out_write) = pipe()?;
    let mut hpc: HPCON = 0;
    // SAFETY: valid pipe handles and out pointer.
    hresult(
        unsafe {
            (api.create)(
                coord(size),
                in_read.as_raw_handle(),
                out_write.as_raw_handle(),
                flags(),
                &mut hpc,
            )
        },
        "CreatePseudoConsole",
    )?;
    let console = Console(hpc);
    // The pseudoconsole holds its own duplicates; ours would keep the output pipe open.
    drop(in_read);
    drop(out_write);

    let mut attrs = AttributeList::new(1)?;
    // SAFETY: the attribute value is the HPCON itself (not a pointer to it), as documented;
    // nothing to keep alive.
    unsafe {
        attrs.set(
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            console.0 as *const std::ffi::c_void,
            std::mem::size_of::<HPCON>(),
        )?;
    }

    // SAFETY: STARTUPINFOEXW is plain old data.
    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    // Explicitly invalid std handles: otherwise the child may inherit the daemon's (null)
    // handles instead of attaching to the pseudoconsole.
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
    si.lpAttributeList = attrs.as_ptr();

    // SAFETY: plain old data out struct.
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: NUL-terminated mutable command line, environment block and cwd; startup info
    // with an initialized attribute list; no inherited handles.
    let created = unsafe {
        CreateProcessW(
            null(),
            line.as_mut_ptr(),
            null(),
            null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
            env.as_ptr().cast(),
            cwd.as_ptr(),
            &si.StartupInfo,
            &mut pi,
        )
    };
    if created == 0 {
        return Err(Error::Spawn {
            program: program.display().to_string(),
            source: io::Error::last_os_error(),
        });
    }
    let process = owned(pi.hProcess, "CreateProcessW")?;
    let thread = owned(pi.hThread, "CreateProcessW")?;

    let job = match kill_on_close_job(&process) {
        Ok(job) => job,
        Err(e) => {
            // SAFETY: our suspended child; it never ran.
            unsafe { TerminateProcess(process.as_raw_handle(), 1) };
            return Err(e);
        }
    };
    // SAFETY: valid thread handle of the suspended child.
    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
        let e = Error::last_os("ResumeThread");
        // SAFETY: valid job handle.
        unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
        return Err(e);
    }

    Ok(Spawned {
        reader: PtyReader(File::from(out_read)),
        writer: PtyWriter(File::from(in_write)),
        child: PtyChild {
            api,
            console: Mutex::new(Some(console)),
            process,
            job,
            pid: pi.dwProcessId,
        },
    })
}

fn kill_on_close_job(process: &OwnedHandle) -> Result<OwnedHandle> {
    // SAFETY: anonymous job with default security.
    let job = owned(
        unsafe { CreateJobObjectW(null(), null()) },
        "CreateJobObjectW",
    )?;
    // SAFETY: plain old data.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: valid job handle and a correctly sized information struct.
    check(
        unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        },
        "SetInformationJobObject",
    )?;
    // SAFETY: valid job and process handles.
    check(
        unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) },
        "AssignProcessToJobObject",
    )?;
    Ok(job)
}

/// Pseudoconsole output.
pub struct PtyReader(File);

impl Read for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // std maps ERROR_BROKEN_PIPE on pipes to Ok(0).
        self.0.read(buf)
    }
}

/// Pseudoconsole input.
pub struct PtyWriter(File);

impl Write for PtyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Control handle: pseudoconsole, process and its job.
pub struct PtyChild {
    api: &'static ConPty,
    console: Mutex<Option<Console>>,
    process: OwnedHandle,
    job: OwnedHandle,
    pid: u32,
}

impl PtyChild {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// No tty path on Windows.
    pub fn tty(&self) -> Option<&str> {
        None
    }

    pub fn resize(&self, size: PtySize) -> Result<()> {
        let console = self.console.lock().unwrap_or_else(PoisonError::into_inner);
        match console.as_ref() {
            // SAFETY: open pseudoconsole (closing takes the same lock).
            Some(c) => hresult(
                unsafe { (self.api.resize)(c.0, coord(size)) },
                "ResizePseudoConsole",
            ),
            None => Ok(()),
        }
    }

    /// Blocks until the child exits; its exit code. Call from exactly one thread.
    pub fn wait(&self) -> Option<i32> {
        let h = self.process.as_raw_handle();
        // SAFETY: valid process handle.
        unsafe { WaitForSingleObject(h, INFINITE) };
        let mut code = 0u32;
        // SAFETY: valid process handle and out pointer.
        (unsafe { GetExitCodeProcess(h, &mut code) } != 0).then_some(code as i32)
    }

    /// Terminates every process in the job immediately (no grace period on Windows).
    pub fn kill_tree(&self, _grace: Duration) {
        // SAFETY: valid job handle.
        unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) };
    }

    /// Closes the pseudoconsole; its output pipe then ends once drained by the reader.
    pub fn close_console(&self) {
        let console = self
            .console
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(c) = console {
            // SAFETY: open pseudoconsole, closed exactly once (taken out of the Option).
            unsafe { (self.api.close)(c.0) };
        }
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        self.close_console();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_block_is_sorted_case_insensitive_and_applies_changes() {
        let spec = SessionSpec {
            name: "t".into(),
            program: "cmd.exe".into(),
            args: Vec::new(),
            cwd: std::path::Path::new(r"C:\").into(),
            env_set: vec![
                ("Omp_Deck_Probe".into(), "1".into()),
                ("path".into(), r"C:\x".into()),
            ],
            env_remove: vec!["windir".into()],
            tsp: false,
        };
        let (block, path, _) = environment(&spec);
        assert_eq!(path.as_deref(), Some(r"C:\x"));
        assert_eq!(&block[block.len() - 2..], &[0, 0]);
        let text = String::from_utf16_lossy(&block[..block.len() - 2]);
        let names: Vec<String> = text
            .split('\0')
            .map(|kv| kv.split('=').next().unwrap_or("").to_uppercase())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        assert!(text.split('\0').any(|kv| kv == "Omp_Deck_Probe=1"));
        assert!(!names.iter().any(|n| n == "WINDIR"));
        assert_eq!(names.iter().filter(|n| *n == "PATH").count(), 1);
    }
}
