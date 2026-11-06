//! Fixed Ctrl+V via the standard virtual-keyboard protocol. No text, shell,
//! activation, uinput, global configuration change, or retry enters this module.
use std::{
    collections::BTreeMap,
    ffi::{CStr, c_char, c_void},
    io::{self, Read},
    mem,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const LIMIT: usize = 128 * 1024;
const KEY_V: u32 = 47;
const CTRL: u32 = 4;
static KEYMAP: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();

pub(super) fn prepare() {
    static START: std::sync::Once = std::sync::Once::new();
    // Fixed-keymap preparation never connects to a display or starts a process.
    // Paste reads only a completed cache; pending/failed preparation is manual.
    START.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("voice-keymap".into())
            .spawn(|| {
                let _ = KEYMAP.set(keymap().ok());
            });
    });
}

/// Prepared local keymap only. Protocol support and actual paste consumption
/// still require the explicit guarded own-window readback probe.
pub(super) fn prepared() -> bool {
    KEYMAP.get().is_some_and(Option::is_some)
}

const BUDGET: Duration = Duration::from_millis(500);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PasteError {
    NotSent,
    Unknown,
}

#[link(name = "xkbcommon")]
unsafe extern "C" {
    fn xkb_context_new(flags: u32) -> *mut c_void;
    fn xkb_context_unref(ctx: *mut c_void);
    fn xkb_keymap_new_from_string(
        ctx: *mut c_void,
        text: *const c_char,
        format: u32,
        flags: u32,
    ) -> *mut c_void;
    fn xkb_keymap_unref(keymap: *mut c_void);
    fn xkb_keymap_get_as_string(keymap: *mut c_void, format: u32) -> *mut c_char;
    fn xkb_keymap_mod_get_index(keymap: *mut c_void, name: *const c_char) -> u32;
    fn xkb_keymap_key_get_syms_by_level(
        keymap: *mut c_void,
        key: u32,
        layout: u32,
        level: u32,
        syms: *mut *const u32,
    ) -> i32;
    fn xkb_context_set_log_level(ctx: *mut c_void, level: i32);
}

fn failed() -> io::Error {
    io::Error::other("virtual keyboard unavailable")
}

fn keymap() -> io::Result<Vec<u8>> {
    struct Context(*mut c_void);
    impl Drop for Context {
        fn drop(&mut self) {
            unsafe { xkb_context_unref(self.0) };
        }
    }
    struct Keymap(*mut c_void);
    impl Drop for Keymap {
        fn drop(&mut self) {
            unsafe { xkb_keymap_unref(self.0) };
        }
    }
    struct Text(*mut c_char);
    impl Drop for Text {
        fn drop(&mut self) {
            unsafe { libc::free(self.0.cast()) };
        }
    }
    // Owned C objects remain alive until serialization and symbol checks finish.
    unsafe {
        let context = xkb_context_new(0);
        if context.is_null() {
            return Err(failed());
        }
        let context = Context(context);
        xkb_context_set_log_level(context.0, 10); // CRITICAL; no model/user data.
        let source = c"xkb_keymap { xkb_keycodes { include \"evdev+aliases(qwerty)\" }; xkb_types { include \"complete\" }; xkb_compatibility { include \"complete\" }; xkb_symbols { include \"pc+us+inet(evdev)\" }; };";
        let map = xkb_keymap_new_from_string(context.0, source.as_ptr(), 1, 0);
        if map.is_null() {
            return Err(failed());
        }
        let map = Keymap(map);
        let mut symbols = std::ptr::null();
        if xkb_keymap_key_get_syms_by_level(map.0, KEY_V + 8, 0, 0, &mut symbols) != 1
            || symbols.is_null()
            || *symbols != 0x76
            || xkb_keymap_mod_get_index(map.0, c"Control".as_ptr()) != 2
        {
            return Err(failed());
        }
        let text = xkb_keymap_get_as_string(map.0, 1);
        if text.is_null() {
            return Err(failed());
        }
        let text = Text(text);
        // The library serializes only the fixed local keymap, never user text.
        let bytes = CStr::from_ptr(text.0).to_bytes_with_nul();
        if bytes.len() > LIMIT {
            return Err(failed());
        }
        Ok(bytes.to_vec())
    }
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
}

fn wait(fd: i32, events: i16, deadline: Instant) -> io::Result<()> {
    loop {
        let millis = remaining(deadline)?.as_millis().clamp(1, i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, millis) };
        if result > 0 {
            return if descriptor.revents & events != 0 {
                Ok(())
            } else {
                Err(failed())
            };
        }
        if result == 0 {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn connect(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { mem::zeroed() };
    if bytes.is_empty() || bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
        return Err(failed());
    }
    address.sun_family = libc::AF_UNIX as _;
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as _;
    }
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    remaining(deadline)?;
    let length = mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    let result = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            length as _,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error);
        }
        wait(fd.as_raw_fd(), libc::POLLOUT, deadline)?;
        let mut status: libc::c_int = 0;
        let mut length = mem::size_of_val(&status) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut status as *mut libc::c_int).cast(),
                &mut length,
            )
        } != 0
            || status != 0
        {
            return Err(failed());
        }
    }
    Ok(UnixStream::from(fd))
}

fn words(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_ne_bytes()).collect()
}
fn frame(object: u32, opcode: u32, payload: &[u8]) -> Vec<u8> {
    let mut result = words(&[object, ((payload.len() as u32 + 8) << 16) | opcode]);
    result.extend_from_slice(payload);
    result
}
fn wire_string(text: &str) -> Vec<u8> {
    let mut result = words(&[text.len() as u32 + 1]);
    result.extend_from_slice(text.as_bytes());
    result.push(0);
    while !result.len().is_multiple_of(4) {
        result.push(0);
    }
    result
}
fn number(bytes: &[u8], offset: usize) -> io::Result<u32> {
    Ok(u32::from_ne_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or_else(failed)?
            .try_into()
            .map_err(|_| failed())?,
    ))
}

struct Connection {
    stream: UnixStream,
    deadline: Instant,
    pending: Vec<u8>,
    received: usize,
    next_id: u32,
    globals: BTreeMap<String, u32>,
    possible_input: bool,
    partial_frame: bool,
    key_down: bool,
    modifiers: bool,
    keyboard: bool,
}
impl Connection {
    fn new(stream: UnixStream, deadline: Instant) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            deadline,
            pending: Vec::new(),
            received: 0,
            next_id: 3,
            globals: BTreeMap::new(),
            possible_input: false,
            partial_frame: false,
            key_down: false,
            modifiers: false,
            keyboard: false,
        })
    }
    fn send(
        &mut self,
        object: u32,
        opcode: u32,
        payload: &[u8],
        fd: Option<i32>,
        input: bool,
    ) -> io::Result<()> {
        let data = frame(object, opcode, payload);
        let mut offset = 0;
        let mut fd = fd;
        while offset < data.len() {
            remaining(self.deadline)?;
            // Aligned ancillary storage remains alive for sendmsg; SCM_RIGHTS
            // is sent exactly once even if the stream accepts only a prefix.
            let control_size = unsafe { libc::CMSG_SPACE(mem::size_of::<i32>() as _) } as usize;
            let mut control = vec![0usize; control_size.div_ceil(mem::size_of::<usize>())];
            let mut vector = libc::iovec {
                iov_base: data[offset..].as_ptr() as *mut _,
                iov_len: data.len() - offset,
            };
            let mut header: libc::msghdr = unsafe { mem::zeroed() };
            header.msg_iov = &mut vector;
            header.msg_iovlen = 1;
            if let Some(fd) = fd {
                header.msg_control = control.as_mut_ptr().cast();
                header.msg_controllen = control_size;
                unsafe {
                    let cmsg = libc::CMSG_FIRSTHDR(&header);
                    (*cmsg).cmsg_level = libc::SOL_SOCKET;
                    (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                    (*cmsg).cmsg_len = libc::CMSG_LEN(mem::size_of::<i32>() as _) as _;
                    std::ptr::copy_nonoverlapping(
                        (&fd as *const i32).cast::<u8>(),
                        libc::CMSG_DATA(cmsg),
                        mem::size_of::<i32>(),
                    );
                }
            }
            let sent = unsafe {
                libc::sendmsg(
                    self.stream.as_raw_fd(),
                    &header,
                    libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
                )
            };
            if sent > 0 {
                if input {
                    self.possible_input = true;
                }
                offset += sent as usize;
                self.partial_frame = offset < data.len();
                fd = None;
            } else if sent == 0 {
                return Err(failed());
            } else {
                let error = io::Error::last_os_error();
                match error.kind() {
                    io::ErrorKind::Interrupted => continue,
                    io::ErrorKind::WouldBlock => {
                        wait(self.stream.as_raw_fd(), libc::POLLOUT, self.deadline)?
                    }
                    _ => return Err(error),
                }
            }
        }
        Ok(())
    }
    fn roundtrip(&mut self) -> io::Result<()> {
        let callback = self.next_id;
        self.next_id += 1;
        self.send(1, 0, &words(&[callback]), None, false)?;
        loop {
            remaining(self.deadline)?;
            if self.pending.len() >= 8 {
                let object = number(&self.pending, 0)?;
                let word = number(&self.pending, 4)?;
                let size = (word >> 16) as usize;
                let opcode = word & 0xffff;
                if !(8..=65535).contains(&size) || !size.is_multiple_of(4) {
                    return Err(failed());
                }
                if self.pending.len() >= size {
                    let payload = self.pending[8..size].to_vec();
                    self.pending.drain(..size);
                    if object == 1 && opcode == 0 {
                        return Err(failed());
                    }
                    if object == 2 && opcode == 0 {
                        let name = number(&payload, 0)?;
                        let len = number(&payload, 4)? as usize;
                        if len == 0 || len > 256 {
                            return Err(failed());
                        }
                        let text = payload.get(8..8 + len).ok_or_else(failed)?;
                        if text.last() != Some(&0) {
                            return Err(failed());
                        }
                        let version = number(&payload, 8 + len.div_ceil(4) * 4)?;
                        let interface =
                            std::str::from_utf8(&text[..len - 1]).map_err(|_| failed())?;
                        if matches!(interface, "wl_seat" | "zwp_virtual_keyboard_manager_v1")
                            && (version < 1
                                || self.globals.insert(interface.into(), name).is_some())
                        {
                            return Err(failed());
                        }
                    }
                    if object == callback && opcode == 0 {
                        if payload.len() != 4 {
                            return Err(failed());
                        }
                        return Ok(());
                    }
                    continue;
                }
            }
            let mut data = [0; 4096];
            match self.stream.read(&mut data) {
                Ok(0) => return Err(failed()),
                Ok(count) => {
                    self.received += count;
                    if self.received > LIMIT {
                        return Err(failed());
                    }
                    self.pending.extend_from_slice(&data[..count]);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    wait(self.stream.as_raw_fd(), libc::POLLIN, self.deadline)?
                }
                Err(error) => return Err(error),
            }
        }
    }
    fn setup(&mut self, keymap: &[u8]) -> io::Result<()> {
        self.send(1, 1, &words(&[2]), None, false)?;
        self.roundtrip()?;
        self.next_id = 7; // registry2, sync3, manager4, seat5, keyboard6
        for (interface, id) in [("zwp_virtual_keyboard_manager_v1", 4), ("wl_seat", 5)] {
            let name = *self.globals.get(interface).ok_or_else(failed)?;
            let mut payload = words(&[name]);
            payload.extend(wire_string(interface));
            payload.extend(words(&[1, id]));
            self.send(2, 0, &payload, None, false)?;
        }
        self.send(4, 0, &words(&[5, 6]), None, false)?;
        self.keyboard = true;
        let fd = unsafe { libc::memfd_create(c"voice-input-keymap".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut file = std::fs::File::from(fd);
        std::io::Write::write_all(&mut file, keymap)?;
        std::io::Seek::rewind(&mut file)?;
        self.send(
            6,
            0,
            &words(&[1, keymap.len() as u32]),
            Some(file.as_raw_fd()),
            false,
        )?;
        self.roundtrip()
    }
    fn inject(&mut self) -> io::Result<()> {
        // A write prefix may already have reached the compositor on failure.
        // Set cleanup flags before attempting each corresponding message.
        self.modifiers = true;
        self.send(6, 2, &words(&[CTRL, 0, 0, 0]), None, true)?;
        self.key_down = true;
        self.send(6, 1, &words(&[0, KEY_V, 1]), None, true)?;
        self.send(6, 1, &words(&[0, KEY_V, 0]), None, true)?;
        self.key_down = false;
        self.send(6, 2, &words(&[0, 0, 0, 0]), None, true)?;
        self.modifiers = false;
        self.send(6, 3, &[], None, false)?;
        self.keyboard = false;
        self.roundtrip()
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        // Best effort only: never wait past the operation deadline, never press
        // a key in cleanup, and never reconnect/retry an input attempt.
        if !self.keyboard || self.partial_frame {
            return;
        }
        let mut data = Vec::new();
        if self.possible_input && self.key_down {
            data.extend(frame(6, 1, &words(&[0, KEY_V, 0])));
        }
        if self.possible_input && self.modifiers {
            data.extend(frame(6, 2, &words(&[0, 0, 0, 0])));
        }
        data.extend(frame(6, 3, &[]));
        unsafe {
            libc::send(
                self.stream.as_raw_fd(),
                data.as_ptr().cast(),
                data.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            );
        }
    }
}

fn paste_stream(
    stream: UnixStream,
    deadline: Instant,
    keymap: &[u8],
    check_focus: impl FnOnce(Instant) -> bool,
) -> Result<(), PasteError> {
    let mut connection = Connection::new(stream, deadline).map_err(|_| PasteError::NotSent)?;
    let result = (|| {
        connection.setup(keymap)?;
        remaining(deadline)?;
        if !check_focus(deadline) {
            return Err(failed());
        }
        remaining(deadline)?;
        connection.inject()
    })();
    result.map_err(|_| {
        if connection.possible_input {
            PasteError::Unknown
        } else {
            PasteError::NotSent
        }
    })
}

pub(super) fn paste(check_focus: impl FnOnce(Instant) -> bool) -> Result<(), PasteError> {
    let deadline = Instant::now() + BUDGET;
    let keymap = KEYMAP
        .get()
        .and_then(Option::as_ref)
        .ok_or(PasteError::NotSent)?;
    let display = std::env::var_os("WAYLAND_DISPLAY").ok_or(PasteError::NotSent)?;
    let display = PathBuf::from(display);
    let path = if display.is_absolute() {
        display
    } else {
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or(PasteError::NotSent)?).join(display)
    };
    let stream = connect(&path, deadline).map_err(|_| PasteError::NotSent)?;
    paste_stream(stream, deadline, keymap, check_focus)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };

    #[derive(Clone, Copy)]
    enum Mode {
        Normal,
        MissingManager,
        Malformed,
        NoFinalAck,
        CloseAfterPress,
    }
    struct Observation {
        inputs: Vec<(u32, Vec<u32>)>,
        keymap_fd: bool,
    }

    // Entirely local socket pair; never connects to the desktop or injects keys.
    fn server(mut stream: UnixStream, mode: Mode, ready: Arc<AtomicBool>) -> Observation {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut observation = Observation {
            inputs: Vec::new(),
            keymap_fd: false,
        };
        let mut syncs = 0;
        loop {
            let mut header = [0u8; 8];
            let mut control = [0usize; 8];
            let mut vector = libc::iovec {
                iov_base: header.as_mut_ptr().cast(),
                iov_len: 8,
            };
            let mut message: libc::msghdr = unsafe { mem::zeroed() };
            message.msg_iov = &mut vector;
            message.msg_iovlen = 1;
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = mem::size_of_val(&control);
            let count = unsafe {
                libc::recvmsg(
                    stream.as_raw_fd(),
                    &mut message,
                    libc::MSG_WAITALL | libc::MSG_CMSG_CLOEXEC,
                )
            };
            if count <= 0 {
                break;
            }
            assert_eq!(count, 8);
            let mut descriptors = Vec::new();
            unsafe {
                let mut cmsg = libc::CMSG_FIRSTHDR(&message);
                while !cmsg.is_null() {
                    assert_eq!((*cmsg).cmsg_level, libc::SOL_SOCKET);
                    assert_eq!((*cmsg).cmsg_type, libc::SCM_RIGHTS);
                    let fd = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<i32>());
                    descriptors.push(OwnedFd::from_raw_fd(fd));
                    cmsg = libc::CMSG_NXTHDR(&message, cmsg);
                }
            }
            let object = number(&header, 0).unwrap();
            let word = number(&header, 4).unwrap();
            let opcode = word & 65535;
            let mut payload = vec![0u8; (word >> 16) as usize - 8];
            stream.read_exact(&mut payload).unwrap();
            if object == 6 && opcode == 0 {
                assert_eq!(descriptors.len(), 1);
                let mut file = std::fs::File::from(descriptors.pop().unwrap());
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).unwrap();
                assert_eq!(bytes.len(), number(&payload, 4).unwrap() as usize);
                assert!(bytes.starts_with(b"xkb_keymap"));
                assert_eq!(bytes.last(), Some(&0));
                observation.keymap_fd = true;
            } else {
                assert!(descriptors.is_empty());
            }
            if object == 6 && matches!(opcode, 1 | 2) {
                let values = payload
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| u32::from_ne_bytes(*b))
                    .collect::<Vec<_>>();
                let press = opcode == 1 && values == [0, KEY_V, 1];
                observation.inputs.push((opcode, values));
                if press && matches!(mode, Mode::CloseAfterPress) {
                    break;
                }
            }
            if object == 1 && opcode == 0 {
                syncs += 1;
                if syncs == 1 {
                    if matches!(mode, Mode::Malformed) {
                        stream.write_all(&words(&[2, 4 << 16])).unwrap();
                        break;
                    }
                    for (name, interface) in
                        [(40, "wl_seat"), (41, "zwp_virtual_keyboard_manager_v1")]
                    {
                        if interface.starts_with("zwp") && matches!(mode, Mode::MissingManager) {
                            continue;
                        }
                        let mut global = words(&[name]);
                        global.extend(wire_string(interface));
                        global.extend(words(&[1]));
                        stream.write_all(&frame(2, 0, &global)).unwrap();
                    }
                }
                if syncs == 2 {
                    ready.store(true, Ordering::SeqCst);
                }
                if syncs >= 3 && matches!(mode, Mode::NoFinalAck) {
                    continue;
                }
                if stream
                    .write_all(&frame(number(&payload, 0).unwrap(), 0, &words(&[0])))
                    .is_err()
                {
                    break;
                }
            }
        }
        observation
    }

    fn exercise(mode: Mode, allow: bool) -> (Result<(), PasteError>, Observation) {
        let (client, peer) = UnixStream::pair().unwrap();
        let ready = Arc::new(AtomicBool::new(false));
        let shared = Arc::clone(&ready);
        let server = thread::spawn(move || server(peer, mode, shared));
        let map = keymap().unwrap();
        let result = paste_stream(
            client,
            Instant::now() + Duration::from_millis(150),
            &map,
            |_| {
                assert!(
                    ready.load(Ordering::SeqCst),
                    "guard must occur after setup acknowledgement"
                );
                allow
            },
        );
        (result, server.join().unwrap())
    }

    #[test]
    fn standard_map_fd_and_single_ordered_paste_are_valid() {
        let (result, observed) = exercise(Mode::Normal, true);
        assert_eq!(result, Ok(()));
        assert!(observed.keymap_fd);
        assert_eq!(
            observed.inputs,
            vec![
                (2, vec![CTRL, 0, 0, 0]),
                (1, vec![0, KEY_V, 1]),
                (1, vec![0, KEY_V, 0]),
                (2, vec![0, 0, 0, 0])
            ]
        );
    }
    #[test]
    fn changed_focus_after_setup_sends_no_input() {
        let (result, observed) = exercise(Mode::Normal, false);
        assert_eq!(result, Err(PasteError::NotSent));
        assert!(observed.keymap_fd);
        assert!(observed.inputs.is_empty());
    }
    #[test]
    fn unsupported_or_malformed_protocol_is_manual_without_input() {
        for mode in [Mode::MissingManager, Mode::Malformed] {
            let (result, observed) = exercise(mode, true);
            assert_eq!(result, Err(PasteError::NotSent));
            assert!(observed.inputs.is_empty());
        }
    }
    #[test]
    fn disconnect_or_missing_ack_after_input_is_unknown_and_never_repeats_press() {
        for mode in [Mode::CloseAfterPress, Mode::NoFinalAck] {
            let before = Instant::now();
            let (result, observed) = exercise(mode, true);
            assert_eq!(result, Err(PasteError::Unknown));
            assert_eq!(
                observed
                    .inputs
                    .iter()
                    .filter(|(op, v)| *op == 1 && v.get(2) == Some(&1))
                    .count(),
                1
            );
            assert!(before.elapsed() < Duration::from_secs(1));
        }
    }
    #[test]
    fn expired_budget_does_not_call_guard_or_send_input() {
        let (client, mut peer) = UnixStream::pair().unwrap();
        let result = paste_stream(
            client,
            Instant::now() - Duration::from_millis(1),
            &[],
            |_| panic!("expired guard"),
        );
        assert_eq!(result, Err(PasteError::NotSent));
        let mut bytes = Vec::new();
        peer.read_to_end(&mut bytes).unwrap();
        assert!(bytes.is_empty());
    }
    #[test]
    fn cleanup_after_no_input_or_partial_frame_never_injects() {
        for (input, partial) in [(false, false), (true, true)] {
            let (client, mut peer) = UnixStream::pair().unwrap();
            let mut connection = Connection::new(client, Instant::now() + BUDGET).unwrap();
            connection.keyboard = true;
            connection.modifiers = true;
            connection.key_down = true;
            connection.possible_input = input;
            connection.partial_frame = partial;
            drop(connection);
            let mut bytes = Vec::new();
            peer.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, if partial { vec![] } else { frame(6, 3, &[]) });
        }
    }
}
