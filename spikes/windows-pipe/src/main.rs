#[cfg(not(windows))]
fn main() {
    eprintln!("the named-pipe security spike runs only on Windows");
}

#[cfg(windows)]
fn main() -> std::io::Result<()> {
    if let Some(path) = std::env::args_os().nth(2)
        && std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--unc-client"))
    {
        return windows::run_unc_client(path);
    }
    windows::run()
}

#[cfg(windows)]
mod windows {
    use interprocess::{
        local_socket::{
            GenericFilePath, GenericNamespaced, Listener, ListenerOptions, Stream, prelude::*,
        },
        os::windows::{local_socket::ListenerOptionsExt, security_descriptor::SecurityDescriptor},
    };
    use std::{
        env,
        ffi::{OsStr, OsString},
        io::{self, BufRead, BufReader, Write},
        os::windows::{ffi::OsStrExt, io::AsRawHandle},
        process::{Command, ExitStatus},
        ptr, thread,
        time::{Duration, Instant},
    };
    use widestring::U16CString;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, LocalFree},
        Security::{
            Authorization::ConvertSidToStringSidW, GetTokenInformation, ImpersonateLoggedOnUser,
            LOGON32_LOGON_NETWORK, LOGON32_PROVIDER_DEFAULT, LogonUserW, RevertToSelf, TOKEN_QUERY,
            TOKEN_USER, TokenUser,
        },
        System::{
            Pipes::{GetNamedPipeInfo, PIPE_REJECT_REMOTE_CLIENTS},
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
    };

    pub fn run() -> io::Result<()> {
        let pipe_name = format!("plato-agent-spike-{}", std::process::id());
        let descriptor = U16CString::from_str(format!("D:P(A;;GA;;;{})", current_user_sid()?))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let descriptor = SecurityDescriptor::deserialize(&descriptor)?;
        let listener = ListenerOptions::new()
            .name(pipe_name.clone().to_ns_name::<GenericNamespaced>()?)
            .security_descriptor(descriptor)
            .create_sync()?;

        prove_remote_clients_are_disabled(&listener)?;
        prove_current_user_connects(&listener, &pipe_name)?;
        prove_second_user_is_rejected(&pipe_name)?;
        prove_remote_path_is_rejected(&pipe_name)?;
        println!("current-user DACL, second-user rejection, and remote rejection passed");
        Ok(())
    }

    pub fn run_unc_client(path: OsString) -> io::Result<()> {
        match Stream::connect(path.to_fs_name::<GenericFilePath>()?) {
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "remote named-pipe client unexpectedly connected",
            )),
            Err(_) => Ok(()),
        }
    }

    fn prove_remote_clients_are_disabled(listener: &Listener) -> io::Result<()> {
        let Listener::NamedPipe(listener) = listener;
        let mut flags = 0;
        let ok = unsafe {
            GetNamedPipeInfo(
                listener.inner().as_raw_handle(),
                &mut flags,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if flags & PIPE_REJECT_REMOTE_CLIENTS == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "named pipe does not report PIPE_REJECT_REMOTE_CLIENTS",
            ));
        }
        Ok(())
    }

    fn prove_current_user_connects(
        listener: &interprocess::local_socket::Listener,
        pipe_name: &str,
    ) -> io::Result<()> {
        let client_name = pipe_name.to_owned();
        let client = thread::spawn(move || -> io::Result<()> {
            let mut stream = Stream::connect(client_name.to_ns_name::<GenericNamespaced>()?)?;
            stream.write_all(b"ping\n")?;
            let mut response = String::new();
            BufReader::new(stream).read_line(&mut response)?;
            if response != "pong\n" {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "authorized client received the wrong response",
                ));
            }
            Ok(())
        });

        let mut stream = BufReader::new(listener.accept()?);
        let mut request = String::new();
        stream.read_line(&mut request)?;
        if request != "ping\n" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "authorized client sent the wrong request",
            ));
        }
        stream.get_mut().write_all(b"pong\n")?;
        client
            .join()
            .map_err(|_| io::Error::other("authorized client panicked"))??;
        Ok(())
    }

    fn prove_second_user_is_rejected(pipe_name: &str) -> io::Result<()> {
        let username = env::var("PLATO_WINDOWS_SECOND_USER")
            .map_err(|_| io::Error::other("PLATO_WINDOWS_SECOND_USER is required"))?;
        let password = env::var("PLATO_WINDOWS_SECOND_PASSWORD")
            .map_err(|_| io::Error::other("PLATO_WINDOWS_SECOND_PASSWORD is required"))?;
        let username = wide(&username);
        let domain = wide(".");
        let password = wide(&password);
        let mut token: HANDLE = ptr::null_mut();
        unsafe {
            if LogonUserW(
                username.as_ptr(),
                domain.as_ptr(),
                password.as_ptr(),
                LOGON32_LOGON_NETWORK,
                LOGON32_PROVIDER_DEFAULT,
                &mut token,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            if ImpersonateLoggedOnUser(token) == 0 {
                let error = io::Error::last_os_error();
                CloseHandle(token);
                return Err(error);
            }
        }

        let result = Stream::connect(pipe_name.to_owned().to_ns_name::<GenericNamespaced>()?);
        let revert = unsafe { RevertToSelf() };
        unsafe {
            CloseHandle(token);
        }
        if revert == 0 {
            return Err(io::Error::last_os_error());
        }
        match result {
            Err(error) if error.raw_os_error() == Some(5) => {}
            Err(error) => {
                return Err(io::Error::other(format!(
                    "second-user connect failed with {error}, expected ERROR_ACCESS_DENIED"
                )));
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "a second local user connected to the pipe",
                ));
            }
        }
        Ok(())
    }

    fn prove_remote_path_is_rejected(pipe_name: &str) -> io::Result<()> {
        let computer =
            env::var("COMPUTERNAME").map_err(|_| io::Error::other("COMPUTERNAME is required"))?;
        let remote_path = format!(r"\\{computer}\pipe\{pipe_name}");
        let mut child = Command::new(env::current_exe()?)
            .arg("--unc-client")
            .arg(remote_path)
            .spawn()?;
        let status = wait_bounded(&mut child, Duration::from_secs(10))?;
        if !status.success() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "a remote named-pipe path connected despite PIPE_REJECT_REMOTE_CLIENTS",
            ));
        }
        Ok(())
    }

    fn wait_bounded(child: &mut std::process::Child, timeout: Duration) -> io::Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                child.kill()?;
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "remote named-pipe probe timed out",
                ));
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn current_user_sid() -> io::Result<String> {
        let mut token: HANDLE = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }

        let result = (|| {
            let mut bytes = 0;
            unsafe {
                GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut bytes);
            }
            if bytes == 0 {
                return Err(io::Error::last_os_error());
            }
            let words = (bytes as usize).div_ceil(std::mem::size_of::<usize>());
            let mut buffer = vec![0usize; words];
            if unsafe {
                GetTokenInformation(
                    token,
                    TokenUser,
                    buffer.as_mut_ptr().cast(),
                    bytes,
                    &mut bytes,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            let user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
            let mut raw = ptr::null_mut();
            if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut raw) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let sid = unsafe {
                let mut len = 0;
                while *raw.add(len) != 0 {
                    len += 1;
                }
                String::from_utf16(std::slice::from_raw_parts(raw, len))
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            };
            unsafe {
                LocalFree(raw.cast());
            }
            sid
        })();
        unsafe {
            CloseHandle(token);
        }
        result
    }

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }
}
