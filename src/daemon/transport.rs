#[cfg(windows)]
use std::io::{Read as _, Write as _};
use std::{io, path::Path};

#[cfg(unix)]
pub(crate) use std::os::unix::net::{UnixListener as Listener, UnixStream as Stream};

#[cfg(windows)]
pub(crate) use interprocess::local_socket::Listener;

#[cfg(windows)]
#[derive(Debug)]
pub(crate) struct Stream {
    inner: WindowsStream,
    deadline: Option<std::time::Instant>,
}

#[cfg(windows)]
#[derive(Debug)]
enum WindowsStream {
    Client(std::fs::File),
    Server(interprocess::local_socket::Stream),
}

#[cfg(all(windows, not(test)))]
const CONTROL_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
#[cfg(all(windows, test))]
const CONTROL_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

#[cfg(unix)]
pub(crate) fn bind(endpoint: &Path) -> io::Result<Listener> {
    Listener::bind(endpoint)
}

#[cfg(windows)]
pub(crate) fn bind(endpoint: &Path) -> io::Result<Listener> {
    use interprocess::{
        local_socket::{GenericFilePath, ListenerOptions, prelude::*},
        os::windows::local_socket::ListenerOptionsExt,
    };

    let descriptor = crate::windows_security::current_user_pipe_descriptor()?;
    ListenerOptions::new()
        .name(endpoint.to_fs_name::<GenericFilePath>()?)
        .security_descriptor(descriptor)
        .create_sync()
}

#[cfg(unix)]
pub(crate) fn connect(endpoint: &Path) -> io::Result<Stream> {
    Stream::connect(endpoint)
}

#[cfg(windows)]
pub(crate) fn connect(endpoint: &Path) -> io::Result<Stream> {
    Ok(Stream {
        inner: WindowsStream::Client(crate::windows_security::connect_current_user_pipe(
            endpoint,
        )?),
        deadline: None,
    })
}

#[cfg(windows)]
pub(crate) fn connect_expected_server(endpoint: &Path, expected_pid: u32) -> io::Result<Stream> {
    Ok(Stream {
        inner: WindowsStream::Client(crate::windows_security::connect_current_user_pipe_for_pid(
            endpoint,
            expected_pid,
        )?),
        deadline: Some(std::time::Instant::now() + CONTROL_IO_TIMEOUT),
    })
}

#[cfg(unix)]
pub(crate) fn accept(listener: &Listener) -> io::Result<Stream> {
    listener.accept().map(|(stream, _)| stream)
}

#[cfg(windows)]
pub(crate) fn accept(listener: &Listener) -> io::Result<Stream> {
    use interprocess::local_socket::prelude::*;

    listener.accept().map(|stream| Stream {
        inner: WindowsStream::Server(stream),
        deadline: None,
    })
}

#[cfg(unix)]
pub(crate) fn try_clone(stream: &Stream) -> io::Result<Stream> {
    stream.try_clone()
}

#[cfg(windows)]
pub(crate) fn reset_deadline(stream: &mut Stream) {
    if stream.deadline.is_some() {
        stream.deadline = Some(std::time::Instant::now() + CONTROL_IO_TIMEOUT);
    }
}

#[cfg(windows)]
pub(crate) fn try_clone(stream: &Stream) -> io::Result<Stream> {
    let inner = match &stream.inner {
        WindowsStream::Client(stream) => WindowsStream::Client(stream.try_clone()?),
        WindowsStream::Server(stream) => {
            WindowsStream::Server(interprocess::TryClone::try_clone(stream)?)
        }
    };
    Ok(Stream {
        inner,
        deadline: stream.deadline,
    })
}

pub(crate) fn wake(endpoint: &Path) {
    let _ = connect(endpoint);
}

#[cfg(windows)]
impl io::Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match &mut self.inner {
            WindowsStream::Client(stream) => read_client(stream, self.deadline, buffer),
            WindowsStream::Server(stream) => stream.read(buffer),
        }
    }
}

#[cfg(windows)]
impl io::Read for &Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match &self.inner {
            WindowsStream::Client(stream) => read_client(stream, self.deadline, buffer),
            WindowsStream::Server(stream) => (&*stream).read(buffer),
        }
    }
}

#[cfg(windows)]
impl io::Write for Stream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match &mut self.inner {
            WindowsStream::Client(stream) => write_client(stream, self.deadline, buffer),
            WindowsStream::Server(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.inner {
            WindowsStream::Client(_) if self.deadline.is_some() => Ok(()),
            WindowsStream::Client(stream) => stream.flush(),
            WindowsStream::Server(stream) => stream.flush(),
        }
    }
}

#[cfg(windows)]
fn read_client(
    mut stream: &std::fs::File,
    deadline: Option<std::time::Instant>,
    buffer: &mut [u8],
) -> io::Result<usize> {
    loop {
        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            return Err(pipe_io_timeout());
        }
        match stream.read(buffer) {
            Err(error)
                if deadline.is_some() && pipe_would_block(&error) && wait_for_pipe(deadline) =>
            {
                continue;
            }
            Err(error) if deadline.is_some() && pipe_would_block(&error) => {
                return Err(pipe_io_timeout());
            }
            result => return result,
        }
    }
}

#[cfg(windows)]
fn write_client(
    mut stream: &std::fs::File,
    deadline: Option<std::time::Instant>,
    buffer: &[u8],
) -> io::Result<usize> {
    loop {
        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            return Err(pipe_io_timeout());
        }
        match stream.write(buffer) {
            Ok(0) if wait_for_pipe(deadline) => continue,
            Ok(0) if deadline.is_some() => return Err(pipe_io_timeout()),
            Err(error)
                if deadline.is_some() && pipe_would_block(&error) && wait_for_pipe(deadline) =>
            {
                continue;
            }
            Err(error) if deadline.is_some() && pipe_would_block(&error) => {
                return Err(pipe_io_timeout());
            }
            result => return result,
        }
    }
}

#[cfg(windows)]
fn pipe_would_block(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
        || error.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_NO_DATA as i32)
}

#[cfg(windows)]
fn wait_for_pipe(deadline: Option<std::time::Instant>) -> bool {
    let Some(deadline) = deadline else {
        return false;
    };
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return false;
    }
    std::thread::sleep(remaining.min(std::time::Duration::from_millis(10)));
    true
}

#[cfg(windows)]
fn pipe_io_timeout() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "named-pipe request timed out")
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        path::PathBuf,
        thread,
    };

    #[test]
    fn named_pipe_round_trip_supports_cloned_streams() {
        let endpoint = PathBuf::from(format!(
            r"\\.\pipe\plato-agent-transport-test-{}",
            std::process::id()
        ));
        let listener = bind(&endpoint).unwrap();
        let client_endpoint = endpoint.clone();
        let client = thread::spawn(move || {
            let stream = connect_expected_server(&client_endpoint, std::process::id()).unwrap();
            let mut writer = try_clone(&stream).unwrap();
            writer.write_all(b"ping").unwrap();
            let mut response = [0; 4];
            let mut reader = &stream;
            reader.read_exact(&mut response).unwrap();
            response
        });

        let stream = accept(&listener).unwrap();
        let mut writer = try_clone(&stream).unwrap();
        let mut request = [0; 4];
        let mut reader = &stream;
        reader.read_exact(&mut request).unwrap();
        writer.write_all(b"pong").unwrap();

        assert_eq!(request, *b"ping");
        assert_eq!(client.join().unwrap(), *b"pong");
    }

    #[test]
    fn named_pipe_rejects_an_unexpected_server_pid() {
        let endpoint = PathBuf::from(format!(
            r"\\.\pipe\plato-agent-transport-pid-test-{}",
            std::process::id()
        ));
        let listener = bind(&endpoint).unwrap();
        let client_endpoint = endpoint.clone();
        let client = thread::spawn(move || {
            connect_expected_server(&client_endpoint, std::process::id().wrapping_add(1))
        });

        let stream = accept(&listener).unwrap();
        drop(stream);
        let error = client.join().unwrap().unwrap_err();

        assert!(
            error
                .to_string()
                .contains("named-pipe server process does not match lock metadata")
        );
    }

    #[test]
    fn expected_server_reads_time_out_without_a_response() {
        let endpoint = PathBuf::from(format!(
            r"\\.\pipe\plato-agent-transport-timeout-test-{}",
            std::process::id()
        ));
        let listener = bind(&endpoint).unwrap();
        let client_endpoint = endpoint.clone();
        let client = thread::spawn(move || {
            let stream = connect_expected_server(&client_endpoint, std::process::id()).unwrap();
            let started = std::time::Instant::now();
            let mut reader = &stream;
            let mut byte = [0; 1];
            let error = reader.read_exact(&mut byte).unwrap_err();
            (started.elapsed(), error)
        });

        let stream = accept(&listener).unwrap();
        thread::sleep(CONTROL_IO_TIMEOUT + std::time::Duration::from_millis(100));
        drop(stream);
        let (elapsed, error) = client.join().unwrap();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(elapsed < CONTROL_IO_TIMEOUT + std::time::Duration::from_millis(100));
    }
}
