//! Own a temporary CoreFP registry pointer for a write or an application session.
//! The elevated half is this same executable, not a permanently installed service.

use std::ffi::{c_void, OsStr};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

type Hkey = *mut c_void;
const HKLM: Hkey = 0x80000002usize as Hkey;
const KEY_QUERY_VALUE: u32 = 0x0001;
const KEY_SET_VALUE: u32 = 0x0002;
const KEY_WOW64_64KEY: u32 = 0x0100;
const REG_SZ: u32 = 1;
const RRF_RT_REG_SZ: u32 = 0x0002;
const RRF_SUBKEY_WOW6464KEY: u32 = 0x0001_0000;
const CORE_FP_READ_FLAGS: u32 = RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const CORE_FP_KEY: &str = r"SOFTWARE\Apple Inc.\CoreFP";
static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueW(
        key: Hkey,
        subkey: *const u16,
        value: *const u16,
        flags: u32,
        kind: *mut u32,
        data: *mut c_void,
        size: *mut u32,
    ) -> i32;
    fn RegCreateKeyExW(
        key: Hkey,
        subkey: *const u16,
        reserved: u32,
        class: *mut u16,
        options: u32,
        access: u32,
        security: *const c_void,
        result: *mut Hkey,
        disposition: *mut u32,
    ) -> i32;
    fn RegSetValueExW(
        key: Hkey,
        value: *const u16,
        reserved: u32,
        kind: u32,
        data: *const u8,
        size: u32,
    ) -> i32;
    fn RegDeleteValueW(key: Hkey, value: *const u16) -> i32;
    fn RegDeleteKeyExW(key: Hkey, subkey: *const u16, sam: u32, reserved: u32) -> i32;
    fn RegQueryInfoKeyW(
        key: Hkey,
        class: *mut u16,
        class_len: *mut u32,
        reserved: *mut u32,
        subkeys: *mut u32,
        max_subkey_len: *mut u32,
        max_class_len: *mut u32,
        values: *mut u32,
        max_value_name_len: *mut u32,
        max_value_len: *mut u32,
        security_len: *mut u32,
        last_write: *mut c_void,
    ) -> i32;
    fn RegCloseKey(key: Hkey) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn ShellExecuteW(
        hwnd: *mut c_void,
        verb: *const u16,
        file: *const u16,
        params: *const u16,
        directory: *const u16,
        show: i32,
    ) -> *mut c_void;
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn key_name() -> Vec<u16> {
    wide(OsStr::new(CORE_FP_KEY))
}
fn value_name() -> Vec<u16> {
    wide(OsStr::new("LibraryPath"))
}

fn prepare_helper_stream(stream: &TcpStream, timeout: Duration) -> Result<()> {
    // On Windows, accept() inherits the listening socket's nonblocking mode.
    // The helper's readiness and cleanup replies may arrive later.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(timeout))?;
    Ok(())
}

fn registry_path() -> Result<Option<PathBuf>> {
    let mut size = 0u32;
    let mut kind = 0u32;
    let status = unsafe {
        RegGetValueW(
            HKLM,
            key_name().as_ptr(),
            value_name().as_ptr(),
            CORE_FP_READ_FLAGS,
            &mut kind,
            ptr::null_mut(),
            &mut size,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if status != 0 {
        bail!("Cannot inspect CoreFP\\LibraryPath (Win32 {status})");
    }
    if kind != REG_SZ {
        bail!("Existing CoreFP\\LibraryPath is not REG_SZ; leaving it unchanged");
    }
    let mut data = vec![0u16; (size as usize).div_ceil(2)];
    let status = unsafe {
        RegGetValueW(
            HKLM,
            key_name().as_ptr(),
            value_name().as_ptr(),
            CORE_FP_READ_FLAGS,
            &mut kind,
            data.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != 0 {
        bail!("Cannot read CoreFP\\LibraryPath (Win32 {status})");
    }
    let end = data.iter().position(|&c| c == 0).unwrap_or(data.len());
    use std::os::windows::ffi::OsStringExt;
    Ok(Some(std::ffi::OsString::from_wide(&data[..end]).into()))
}

struct OpenKey(Hkey);
impl Drop for OpenKey {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

fn insert_if_absent(path: &PathBuf) -> Result<Option<bool>> {
    if registry_path()?.is_some() {
        return Ok(None);
    }
    let mut handle = ptr::null_mut();
    let mut disposition = 0u32;
    let status = unsafe {
        RegCreateKeyExW(
            HKLM,
            key_name().as_ptr(),
            0,
            ptr::null_mut(),
            0,
            KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_WOW64_64KEY,
            ptr::null(),
            &mut handle,
            &mut disposition,
        )
    };
    if status != 0 {
        bail!("Cannot create CoreFP registry key (Win32 {status}); approve the UAC prompt");
    }
    let key = OpenKey(handle);
    // A concurrent writer may have added the value after our first check.
    if registry_path()?.is_some() {
        return Ok(None);
    }
    let data = wide(path.as_os_str());
    let status = unsafe {
        RegSetValueExW(
            key.0,
            value_name().as_ptr(),
            0,
            REG_SZ,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        )
    };
    if status != 0 {
        drop(key);
        if disposition == 1 {
            unsafe {
                RegDeleteKeyExW(HKLM, key_name().as_ptr(), KEY_WOW64_64KEY, 0);
            }
        }
        bail!("Cannot create CoreFP\\LibraryPath (Win32 {status})");
    }
    Ok(Some(disposition == 1))
}

fn remove_ours(path: &PathBuf, created_key: bool) -> Result<()> {
    if registry_path()?.as_ref() != Some(path) {
        return Ok(());
    }
    let mut handle = ptr::null_mut();
    let status = unsafe {
        RegCreateKeyExW(
            HKLM,
            key_name().as_ptr(),
            0,
            ptr::null_mut(),
            0,
            KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_WOW64_64KEY,
            ptr::null(),
            &mut handle,
            ptr::null_mut(),
        )
    };
    if status != 0 {
        bail!("Cannot reopen CoreFP registry key for cleanup (Win32 {status})");
    }
    let key = OpenKey(handle);
    let status = unsafe { RegDeleteValueW(key.0, value_name().as_ptr()) };
    if status != 0 {
        bail!("Cannot remove temporary CoreFP\\LibraryPath (Win32 {status})");
    }
    if created_key {
        let mut subkeys = 0u32;
        let mut values = 0u32;
        let status = unsafe {
            RegQueryInfoKeyW(
                key.0,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut subkeys,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut values,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        drop(key);
        if status == 0 && subkeys == 0 && values == 0 {
            unsafe {
                RegDeleteKeyExW(HKLM, key_name().as_ptr(), KEY_WOW64_64KEY, 0);
            }
        }
    }
    Ok(())
}

fn corefp_dll() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("Cannot locate aircard.exe")?;
    let portable = exe
        .parent()
        .context("Executable has no parent folder")?
        .join("AppleSupport")
        .join("CoreFP.dll");
    if portable.is_file() {
        let coreke = portable.with_file_name("CoreKE.dll");
        if !coreke.is_file() {
            bail!("Portable CoreKE.dll is missing beside CoreFP.dll");
        }
        // Keep the ordinary drive-letter path: some Apple components may not
        // understand the extended \\?\ prefix returned by canonicalize().
        return Ok(portable);
    }
    bail!(
        "CoreFP.dll is missing. Place CoreFP.dll and CoreKE.dll in AppleSupport beside aircard.exe"
    )
}

pub struct WriteGuard {
    pointer: SessionGuard,
    _lock: MutexGuard<'static, ()>,
}

impl WriteGuard {
    pub fn begin() -> Result<Self> {
        let lock = WRITE_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Ok(Self { pointer: SessionGuard::begin_locked()?, _lock: lock })
    }

    pub fn finish(self) -> Result<()> {
        let Self { pointer, _lock } = self;
        let result = pointer.finish();
        drop(_lock);
        result
    }
}

pub struct SessionGuard {
    helper: Option<TcpStream>,
}

impl SessionGuard {
    pub fn begin() -> Result<Self> {
        let _lock = WRITE_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        Self::begin_locked()
    }

    pub fn is_temporary(&self) -> bool {
        self.helper.is_some()
    }

    fn begin_locked() -> Result<Self> {
        if registry_path()?.is_some() {
            return Ok(Self {
                helper: None,
            });
        }
        let dll = corefp_dll()?;
        let listener =
            TcpListener::bind("127.0.0.1:0").context("Cannot start local CoreFP helper channel")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let exe = std::env::current_exe()?;
        let args = format!("--corefp-helper {port} \"{}\"", dll.display());
        let verb = wide(OsStr::new("runas"));
        let file = wide(exe.as_os_str());
        let params = wide(OsStr::new(&args));
        let result = unsafe {
            ShellExecuteW(
                ptr::null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                params.as_ptr(),
                ptr::null(),
                0,
            )
        } as isize;
        if result <= 32 {
            bail!("CoreFP registry permission was not granted (ShellExecute error {result})");
        }
        let until = Instant::now() + Duration::from_secs(45);
        loop {
            match listener.accept() {
                Ok((mut stream, peer)) if peer.ip().is_loopback() => {
                    prepare_helper_stream(&stream, Duration::from_secs(15))?;
                    let mut status = [0u8; 1];
                    stream
                        .read_exact(&mut status)
                        .context("CoreFP helper did not report readiness")?;
                    if status[0] == b'2' {
                        return Ok(Self {
                            helper: None,
                        });
                    }
                    if status[0] != b'1' {
                        bail!("CoreFP helper could not create the temporary registry value");
                    }
                    if registry_path()?.as_ref() != Some(&dll) {
                        bail!("CoreFP helper reported a registry value different from the expected DLL");
                    }
                    return Ok(Self {
                        helper: Some(stream),
                    });
                }
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(err) => return Err(err).context("CoreFP helper channel failed"),
            }
            if Instant::now() >= until {
                bail!("Timed out waiting for the elevated CoreFP helper");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn finish(mut self) -> Result<()> {
        if let Some(mut stream) = self.helper.take() {
            let _ = stream.shutdown(Shutdown::Write);
            let mut result = [0u8; 1];
            stream.set_read_timeout(Some(Duration::from_secs(10)))?;
            stream
                .read_exact(&mut result)
                .context("CoreFP helper did not confirm registry cleanup")?;
            if result[0] != b'1' {
                bail!("CoreFP helper could not clean the temporary registry value");
            }
        }
        Ok(())
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Some(stream) = self.helper.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

pub fn maybe_run_helper() -> Result<bool> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).map(|x| x == "--corefp-helper") != Some(true) {
        return Ok(false);
    }
    if args.len() != 4 {
        bail!("Invalid CoreFP helper arguments");
    }
    let port: u16 = args[2]
        .to_string_lossy()
        .parse()
        .context("Invalid helper port")?;
    let path = PathBuf::from(&args[3]);
    let mut stream =
        TcpStream::connect(("127.0.0.1", port)).context("Cannot connect to AirCard")?;
    let ownership = match insert_if_absent(&path) {
        Ok(created) => created,
        Err(err) => {
            let _ = stream.write_all(b"0");
            return Err(err);
        }
    };
    if ownership.is_none() {
        stream.write_all(b"2")?;
        return Ok(true);
    }
    if stream.write_all(b"1").is_err() {
        remove_ours(&path, ownership.unwrap_or(false))?;
        return Ok(true);
    }
    // The socket closes automatically if the parent crashes, so cleanup still runs.
    let mut buf = [0u8; 1];
    let _ = stream.read(&mut buf);
    let result = remove_ours(&path, ownership.unwrap_or(false));
    let _ = stream.write_all(if result.is_ok() { b"1" } else { b"0" });
    result?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_value_is_not_invalid_parameter() {
        let mut size = 0u32;
        let mut kind = 0u32;
        let missing = wide(OsStr::new("__AirCard_missing_test_value__"));
        let status = unsafe {
            RegGetValueW(
                HKLM,
                key_name().as_ptr(),
                missing.as_ptr(),
                CORE_FP_READ_FLAGS,
                &mut kind,
                ptr::null_mut(),
                &mut size,
            )
        };
        assert_eq!(status, ERROR_FILE_NOT_FOUND);
    }

    #[test]
    fn accepted_helper_stream_waits_for_delayed_reply() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let accepted = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("accept failed: {err}"),
            }
        };
        prepare_helper_stream(&accepted, Duration::from_secs(1)).unwrap();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            client.write_all(b"1").unwrap();
        });
        let mut accepted = accepted;
        let mut reply = [0u8; 1];
        accepted.read_exact(&mut reply).unwrap();
        sender.join().unwrap();
        assert_eq!(reply, *b"1");
    }

    #[test]
    fn session_guard_keeps_helper_alive_until_finish() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut helper, _) = listener.accept().unwrap();
        let worker = std::thread::spawn(move || {
            helper.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut byte = [0u8; 1];
            assert_eq!(helper.read(&mut byte).unwrap(), 0);
            helper.write_all(b"1").unwrap();
        });
        let guard = SessionGuard { helper: Some(client) };
        assert!(guard.is_temporary());
        guard.finish().unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn dropping_session_guard_notifies_helper_of_parent_exit() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut helper, _) = listener.accept().unwrap();
        helper.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        drop(SessionGuard { helper: Some(client) });
        assert_eq!(helper.read(&mut [0u8; 1]).unwrap(), 0);
    }

    #[test]
    fn preexisting_pointer_does_not_have_a_cleanup_helper() {
        let guard = SessionGuard { helper: None };
        assert!(!guard.is_temporary());
        guard.finish().unwrap();
    }
}
