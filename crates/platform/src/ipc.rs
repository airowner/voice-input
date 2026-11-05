//! Private per-login-session Unix control socket. No text or credentials on wire.
use crate::{PlatformError, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    hash::{Hash, Hasher},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const MAX_REQUEST_BYTES: usize = 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
    Start,
    Stop,
    Toggle,
    Cancel,
    Status,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlRequest {
    pub action: ControlAction,
}

pub fn parse_request(bytes: &[u8]) -> Result<ControlRequest> {
    if bytes.is_empty() || bytes.len() > MAX_REQUEST_BYTES {
        return Err(PlatformError::InvalidRequest);
    }
    serde_json::from_slice(bytes).map_err(|_| PlatformError::InvalidRequest)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlState {
    Ready,
    Recording,
    Transcribing,
    Rewriting,
    Delivering,
    Result,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SanitizedStatus {
    pub state: ControlState,
    pub busy: bool,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyCode {
    Accepted,
    Busy,
    Invalid,
    Unavailable,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlReply {
    pub code: ReplyCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<SanitizedStatus>,
}

pub fn session_socket_path() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or(PlatformError::Session)?;
    let session = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .or_else(|_| std::env::var("XDG_SESSION_ID"))
        .map_err(|_| PlatformError::Session)?;
    if session.is_empty() {
        return Err(PlatformError::Session);
    }
    let metadata = fs::symlink_metadata(&runtime).map_err(|_| PlatformError::Session)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(PlatformError::Session);
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    session.hash(&mut hasher);
    Ok(runtime
        .join(format!("voice-input-{:016x}", hasher.finish()))
        .join("control.sock"))
}

/// Keep this value alive for the entire application lifetime. Acquire before
/// registering shortcuts or initializing application IPC.
pub struct SessionSocket {
    listener: UnixListener,
    path: PathBuf,
    _instance_lock: File,
}
impl SessionSocket {
    pub fn bind() -> Result<Self> {
        Self::bind_path(&session_socket_path()?)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Bind an explicit socket path for isolated service environments/tests.
    /// The same private-directory permissions, current-user ownership, peer UID,
    /// symlink rejection and exclusive instance-lock checks remain mandatory.
    pub fn bind_path(path: &Path) -> Result<Self> {
        let directory = path.parent().ok_or(PlatformError::Socket)?;
        if !directory.exists() {
            match DirBuilder::new().mode(0o700).create(directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(PlatformError::Socket),
            }
        }
        let metadata = fs::symlink_metadata(directory).map_err(|_| PlatformError::Socket)?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(PlatformError::Socket);
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory.join("instance.lock"))
            .map_err(|_| PlatformError::Socket)?;
        let lock_metadata = lock.metadata().map_err(|_| PlatformError::Socket)?;
        if !lock_metadata.is_file()
            || lock_metadata.uid() != unsafe { libc::geteuid() }
            || lock_metadata.permissions().mode() & 0o077 != 0
        {
            return Err(PlatformError::Socket);
        }
        lock.try_lock_exclusive()
            .map_err(|_| PlatformError::AlreadyRunning)?;
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() }
                {
                    return Err(PlatformError::Socket);
                }
                fs::remove_file(path).map_err(|_| PlatformError::Socket)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(PlatformError::Socket),
        }
        let listener = UnixListener::bind(path).map_err(|_| PlatformError::Socket)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| PlatformError::Socket)?;
        listener
            .set_nonblocking(true)
            .map_err(|_| PlatformError::Socket)?;
        Ok(Self {
            listener,
            path: path.into(),
            _instance_lock: lock,
        })
    }
    /// Nonblocking accept. Read/process each connection on the caller's serial
    /// control queue; this module never spawns concurrent action dispatchers.
    pub fn accept(&self) -> Result<Option<ControlConnection>> {
        let (stream, _) = match self.listener.accept() {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(_) => return Err(PlatformError::Socket),
        };
        verify_peer(&stream)?;
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .map_err(|_| PlatformError::Socket)?;
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .map_err(|_| PlatformError::Socket)?;
        Ok(Some(ControlConnection {
            stream,
            action: None,
        }))
    }
}
impl Drop for SessionSocket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn verify_peer(stream: &UnixStream) -> Result<()> {
    let mut credential: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credential as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0
        || length as usize != std::mem::size_of::<libc::ucred>()
        || credential.uid != unsafe { libc::geteuid() }
    {
        return Err(PlatformError::Socket);
    }
    Ok(())
}

pub struct ControlConnection {
    stream: UnixStream,
    action: Option<ControlAction>,
}
impl ControlConnection {
    pub fn read_request(&mut self) -> Result<ControlRequest> {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut bytes = Vec::with_capacity(128);
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(PlatformError::Timeout)?;
            self.stream
                .set_read_timeout(Some(remaining))
                .map_err(|_| PlatformError::Socket)?;
            let mut chunk = [0; 256];
            let count = self
                .stream
                .read(&mut chunk)
                .map_err(|_| PlatformError::Timeout)?;
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..count]);
            if bytes.len() > MAX_REQUEST_BYTES {
                return Err(PlatformError::InvalidRequest);
            }
            if bytes.contains(&b'\n') {
                // One request per connection; serde rejects trailing non-whitespace.
                break;
            }
        }
        let request = parse_request(&bytes)?;
        self.action = Some(request.action);
        Ok(request)
    }
    pub fn reply(mut self, mut reply: ControlReply) -> Result<()> {
        // Only status is permitted to query even sanitized application state.
        if self.action != Some(ControlAction::Status) {
            reply.status = None;
        }
        let mut bytes = serde_json::to_vec(&reply).map_err(|_| PlatformError::Socket)?;
        bytes.push(b'\n');
        self.stream
            .write_all(&bytes)
            .map_err(|_| PlatformError::Socket)
    }
}

/// CLI helper. Nonblocking connect is bounded independently at 250 ms;
/// response has a one-second absolute deadline and a 1 KiB cap.
pub fn send_request(request: ControlRequest) -> Result<ControlReply> {
    let path = session_socket_path()?;
    let stream = connect_timeout(&path, Duration::from_millis(250))?;
    verify_peer(&stream)?;
    let mut connection = ControlConnection {
        stream,
        action: Some(request.action),
    };
    connection
        .stream
        .set_write_timeout(Some(Duration::from_secs(1)))
        .map_err(|_| PlatformError::Socket)?;
    let mut bytes = serde_json::to_vec(&request).map_err(|_| PlatformError::InvalidRequest)?;
    bytes.push(b'\n');
    connection
        .stream
        .write_all(&bytes)
        .map_err(|_| PlatformError::Socket)?;
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut response = Vec::new();
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(PlatformError::Timeout)?;
        connection
            .stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| PlatformError::Socket)?;
        let mut chunk = [0; 256];
        let count = connection
            .stream
            .read(&mut chunk)
            .map_err(|_| PlatformError::Timeout)?;
        if count == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..count]);
        if response.len() > MAX_REQUEST_BYTES {
            return Err(PlatformError::OutputLimit);
        }
        if response.contains(&b'\n') {
            break;
        }
    }
    serde_json::from_slice(&response).map_err(|_| PlatformError::InvalidRequest)
}

fn connect_timeout(path: &Path, timeout: Duration) -> Result<UnixStream> {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= address.sun_path.len() {
        return Err(PlatformError::Socket);
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, source) in address.sun_path.iter_mut().zip(bytes.iter()) {
        *target = *source as libc::c_char;
    }
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if raw < 0 {
        return Err(PlatformError::Socket);
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let connected = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if connected != 0 {
        let error = std::io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::EINPROGRESS | libc::EAGAIN)) {
            return Err(PlatformError::Socket);
        }
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, timeout.as_millis() as i32) };
        if ready == 0 {
            return Err(PlatformError::Timeout);
        }
        if ready < 0 {
            return Err(PlatformError::Socket);
        }
        let mut pending: libc::c_int = 0;
        let mut length = std::mem::size_of_val(&pending) as libc::socklen_t;
        let status = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut pending as *mut libc::c_int).cast(),
                &mut length,
            )
        };
        if status != 0 || pending != 0 {
            return Err(PlatformError::Socket);
        }
    }
    let stream = UnixStream::from(fd);
    stream
        .set_nonblocking(false)
        .map_err(|_| PlatformError::Socket)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_rejects_unknown_action_fields_and_trailing_requests() {
        for input in [
            br#"{"action":"quit"}"#.as_slice(),
            br#"{"action":"status","text":"secret"}"#.as_slice(),
            b"{\"action\":\"start\"}\n{\"action\":\"stop\"}".as_slice(),
        ] {
            assert!(parse_request(input).is_err());
        }
        assert_eq!(
            parse_request(b"{\"action\":\"toggle\"}\n").unwrap().action,
            ControlAction::Toggle
        );
        assert!(parse_request(&vec![b'x'; 1025]).is_err());
    }
    #[test]
    fn private_socket_single_instance_and_peer_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private").join("control.sock");
        let server = SessionSocket::bind_path(&path).unwrap();
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(matches!(
            SessionSocket::bind_path(&path),
            Err(PlatformError::AlreadyRunning)
        ));
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"{\"action\":\"start\"}\n").unwrap();
        let mut connection = server.accept().unwrap().unwrap();
        assert_eq!(
            connection.read_request().unwrap().action,
            ControlAction::Start
        );
        connection
            .reply(ControlReply {
                code: ReplyCode::Accepted,
                status: Some(SanitizedStatus {
                    state: ControlState::Ready,
                    busy: false,
                }),
            })
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(!response.contains("status"));
        drop(server);
        assert!(!path.exists());
    }
    #[test]
    fn socket_refuses_nonprivate_directory_and_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let public = temp.path().join("public");
        fs::create_dir(&public).unwrap();
        fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(SessionSocket::bind_path(&public.join("control.sock")).is_err());
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&public, &link).unwrap();
        assert!(SessionSocket::bind_path(&link.join("control.sock")).is_err());
    }
}
