#![allow(unsafe_code)]

use interprocess::os::windows::security_descriptor::{AsSecurityDescriptorExt, SecurityDescriptor};
use std::{
    fs::File,
    io, mem,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Path, PathBuf},
    ptr,
    time::{Duration, Instant},
};
use widestring::U16CString;
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_PIPE_BUSY, ERROR_SEM_TIMEOUT, GENERIC_READ,
        GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree, WAIT_TIMEOUT,
    },
    Security::{
        Authorization::ConvertSidToStringSidW, EqualSid, GetTokenInformation, SECURITY_ATTRIBUTES,
        TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    },
    System::{
        Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW},
        SystemInformation::GetSystemDirectoryW,
        Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SYNCHRONIZE, WaitForSingleObject,
        },
    },
};

pub(crate) fn current_user_pipe_descriptor() -> io::Result<SecurityDescriptor> {
    current_user_descriptor("GA")
}

pub(crate) fn create_current_user_file(path: &Path) -> io::Result<File> {
    let descriptor = current_user_descriptor("FA")?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: mem::size_of::<SECURITY_ATTRIBUTES>()
            .try_into()
            .expect("SECURITY_ATTRIBUTES size fits u32"),
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 0,
    };
    descriptor.write_to_security_attributes(&mut attributes);

    let mut path_wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if path_wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows lock path contains a NUL",
        ));
    }
    path_wide.push(0);

    // SAFETY: path_wide is NUL-terminated, attributes borrows a live descriptor,
    // and the returned owned handle is checked before conversion to File.
    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: CreateFileW returned a new owned handle and File assumes that ownership.
    Ok(unsafe { File::from_raw_handle(handle) })
}

pub(crate) fn connect_current_user_pipe(path: &Path) -> io::Result<File> {
    let path_wide = path_wide(path, "Windows pipe path contains a NUL")?;
    let deadline = Instant::now() + Duration::from_secs(1);
    let handle = loop {
        // SAFETY: path_wide is NUL-terminated; the returned owned handle is checked below.
        let handle = unsafe {
            CreateFileW(
                path_wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null_mut(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            // SAFETY: CreateFileW returned a new owned handle.
            break unsafe { File::from_raw_handle(handle) };
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) {
            return Err(error);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(pipe_connect_timeout());
        }
        let wait_ms = remaining.as_millis().clamp(1, u32::MAX as u128) as u32;
        // SAFETY: path_wide remains a live NUL-terminated string for this call.
        if unsafe { WaitNamedPipeW(path_wide.as_ptr(), wait_ms) } == 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_SEM_TIMEOUT as i32) {
                Err(pipe_connect_timeout())
            } else {
                Err(error)
            };
        }
    };

    validate_pipe_server(&handle)?;
    Ok(handle)
}

fn pipe_connect_timeout() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "named-pipe connection timed out")
}

pub(crate) fn system_cmd_path() -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 260];
    loop {
        // SAFETY: buffer exposes writable UTF-16 storage for its reported capacity.
        let len = unsafe {
            GetSystemDirectoryW(
                buffer.as_mut_ptr(),
                buffer
                    .len()
                    .try_into()
                    .map_err(|_| io::Error::other("Windows system path is too long"))?,
            )
        };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        let len = len as usize;
        if len < buffer.len() {
            let root = std::ffi::OsString::from_wide(&buffer[..len]);
            return Ok(PathBuf::from(root).join("cmd.exe"));
        }
        buffer.resize(len + 1, 0);
    }
}

fn current_user_descriptor(rights: &str) -> io::Result<SecurityDescriptor> {
    let sid = current_user_sid_string()?;
    let descriptor = U16CString::from_str(format!("O:{sid}D:P(A;;{rights};;;{sid})"))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    SecurityDescriptor::deserialize(&descriptor)
}

fn current_user_sid_string() -> io::Result<String> {
    let user = process_user(current_process())?;
    user.sid_string()
}

fn process_user(process: HANDLE) -> io::Result<TokenUserBuffer> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: token points to writable storage and is wrapped immediately on success.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = WinHandle(token);
    let mut bytes = 0;
    // SAFETY: the documented zero-length query writes only the required byte count.
    let result = unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut bytes) };
    if result != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "token size query unexpectedly succeeded",
        ));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
        return Err(error);
    }
    if bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "token size query returned no bytes",
        ));
    }

    let words = (bytes as usize).div_ceil(std::mem::size_of::<usize>());
    let mut buffer = vec![0usize; words];
    // SAFETY: the aligned buffer has at least the byte count returned by the size query.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(TokenUserBuffer { buffer })
}

fn validate_pipe_server(handle: &File) -> io::Result<()> {
    let raw = handle.as_raw_handle();
    let mut first_pid = 0;
    // SAFETY: raw is a live connected pipe handle and first_pid is writable output storage.
    if unsafe { GetNamedPipeServerProcessId(raw, &mut first_pid) } == 0 || first_pid == 0 {
        return Err(pipe_server_identity_error());
    }
    // SAFETY: the returned process handle is checked and wrapped on success.
    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            first_pid,
        )
    };
    if process.is_null() {
        return Err(pipe_server_identity_error());
    }
    let process = WinHandle(process);
    let current_user = process_user(current_process()).map_err(|_| pipe_server_identity_error())?;
    let server_user = process_user(process.0).map_err(|_| pipe_server_identity_error())?;
    // SAFETY: both SID pointers borrow live TOKEN_USER buffers.
    if unsafe { EqualSid(current_user.sid(), server_user.sid()) } == 0 {
        return Err(pipe_server_identity_error());
    }
    let mut second_pid = 0;
    // SAFETY: raw remains live and second_pid is writable output storage.
    if unsafe { GetNamedPipeServerProcessId(raw, &mut second_pid) } == 0 || second_pid != first_pid
    {
        return Err(pipe_server_identity_error());
    }
    // SAFETY: process is a live process handle opened with PROCESS_SYNCHRONIZE.
    if unsafe { WaitForSingleObject(process.0, 0) } != WAIT_TIMEOUT {
        return Err(pipe_server_identity_error());
    }
    Ok(())
}

fn current_process() -> HANDLE {
    // SAFETY: GetCurrentProcess always returns the calling process's pseudo-handle.
    unsafe { GetCurrentProcess() }
}

fn pipe_server_identity_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "named-pipe server is not owned by the current user",
    )
}

fn path_wide(path: &Path, nul_message: &'static str) -> io::Result<Vec<u16>> {
    let mut path_wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if path_wide.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, nul_message));
    }
    path_wide.push(0);
    Ok(path_wide)
}

struct TokenUserBuffer {
    buffer: Vec<usize>,
}

impl TokenUserBuffer {
    fn sid(&self) -> *mut core::ffi::c_void {
        // SAFETY: GetTokenInformation initialized the aligned buffer as TOKEN_USER.
        unsafe { (*(self.buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid }
    }

    fn sid_string(&self) -> io::Result<String> {
        let mut raw = ptr::null_mut();
        // SAFETY: self.sid() belongs to this live token buffer; raw is writable output storage.
        if unsafe { ConvertSidToStringSidW(self.sid(), &mut raw) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if raw.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SID conversion returned a null string",
            ));
        }
        let raw = LocalWideString(raw);
        let mut len = 0;
        // SAFETY: ConvertSidToStringSidW returns a NUL-terminated LocalAlloc string.
        while unsafe { *raw.0.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: len was measured within the API-owned NUL-terminated string.
        String::from_utf16(unsafe { std::slice::from_raw_parts(raw.0, len) })
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

struct WinHandle(HANDLE);

impl Drop for WinHandle {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns a successful Win32 handle.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct LocalWideString(*mut u16);

impl Drop for LocalWideString {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns the successful ConvertSidToStringSidW allocation.
        unsafe {
            LocalFree(self.0.cast());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_sid_is_a_sid_string() {
        assert!(current_user_sid_string().unwrap().starts_with("S-1-"));
    }

    #[test]
    fn creates_current_user_file_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.lock");

        let file = create_current_user_file(&path).unwrap();
        assert!(path.exists());
        let error = create_current_user_file(&path).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        drop(file);
    }

    #[test]
    fn resolves_the_system_command_host() {
        let path = system_cmd_path().unwrap();

        assert_eq!(path.file_name().unwrap(), "cmd.exe");
        assert!(path.is_absolute());
        assert!(path.exists());
    }
}
