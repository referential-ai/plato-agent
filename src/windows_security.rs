#![allow(unsafe_code)]

use interprocess::os::windows::security_descriptor::{AsSecurityDescriptorExt, SecurityDescriptor};
use std::{
    fs::File,
    io, mem,
    os::windows::{ffi::OsStrExt, io::FromRawHandle},
    path::Path,
    ptr,
};
use widestring::U16CString;
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
        LocalFree,
    },
    Security::{
        Authorization::ConvertSidToStringSidW, GetTokenInformation, SECURITY_ATTRIBUTES,
        TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::{CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ},
    System::Threading::{GetCurrentProcess, OpenProcessToken},
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

fn current_user_descriptor(rights: &str) -> io::Result<SecurityDescriptor> {
    let sid = current_user_sid_string()?;
    let descriptor = U16CString::from_str(format!("O:{sid}D:P(A;;{rights};;;{sid})"))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    SecurityDescriptor::deserialize(&descriptor)
}

fn current_user_sid_string() -> io::Result<String> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: token points to writable storage and is wrapped immediately on success.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);

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

    // SAFETY: GetTokenInformation initialized buffer as TOKEN_USER on success.
    let user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
    let mut raw = ptr::null_mut();
    // SAFETY: user.User.Sid belongs to the live token buffer; raw is writable output storage.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut raw) } == 0 {
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

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns the successful OpenProcessToken handle.
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
}
