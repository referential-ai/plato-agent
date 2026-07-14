use std::{io, path::Path};

#[cfg(unix)]
pub(crate) use std::os::unix::net::{UnixListener as Listener, UnixStream as Stream};

#[cfg(windows)]
pub(crate) use interprocess::local_socket::{Listener, Stream};

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
    use interprocess::local_socket::{GenericFilePath, prelude::*};

    Stream::connect(endpoint.to_fs_name::<GenericFilePath>()?)
}

#[cfg(unix)]
pub(crate) fn accept(listener: &Listener) -> io::Result<Stream> {
    listener.accept().map(|(stream, _)| stream)
}

#[cfg(windows)]
pub(crate) fn accept(listener: &Listener) -> io::Result<Stream> {
    use interprocess::local_socket::prelude::*;

    listener.accept()
}

#[cfg(unix)]
pub(crate) fn try_clone(stream: &Stream) -> io::Result<Stream> {
    stream.try_clone()
}

#[cfg(windows)]
pub(crate) fn try_clone(stream: &Stream) -> io::Result<Stream> {
    interprocess::TryClone::try_clone(stream)
}

pub(crate) fn wake(endpoint: &Path) {
    let _ = connect(endpoint);
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
            let stream = connect(&client_endpoint).unwrap();
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
}
