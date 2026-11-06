#!/usr/bin/env python3
"""Controlled M0 desktop probes. Requires an unlocked Hyprland user session.

Creates only owned GTK test windows; never injects into a pre-existing window.
Clipboard contents stay in memory and are restored with all advertised MIME types.
No permanent desktop configuration is written. Run with --output NEW.json.
"""
import argparse
import base64
import datetime
import hashlib
import json
import os
from pathlib import Path
import select
import shlex
import socket
import subprocess
import sys
import tempfile
import time

TEXT = 'M0 中文 English 2026 /tmp/example.txt\n第二行：$ ` \\ " !'
LIMIT = 8 * 1024 * 1024


def command(args, timeout=0.5, data=None):
    p = subprocess.run(args, input=data, capture_output=True, timeout=timeout, check=False)
    if p.returncode:
        detail = p.stderr.decode(errors='replace')[:200]
        if Path(args[0]).name == 'hyprctl':
            detail += p.stdout.decode(errors='replace')[:200]
        raise RuntimeError(f'{Path(args[0]).name}:exit={p.returncode}:' + detail)
    return p.stdout


def read_bounded(args, limit=LIMIT, timeout=0.5):
    p = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    data = bytearray()
    deadline = time.monotonic() + timeout
    try:
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([p.stdout], [], [], remaining)[0]:
                raise TimeoutError('bounded read timed out')
            block = os.read(p.stdout.fileno(), 65536)
            if not block:
                break
            data.extend(block)
            if len(data) > limit:
                raise RuntimeError('clipboard_limit_exceeded')
        if p.wait(timeout=max(0.01, deadline-time.monotonic())):
            raise RuntimeError('source_read_failed')
        return bytes(data)
    finally:
        if p.poll() is None:
            p.kill()
        p.wait()
        p.stdout.close()


def release_keymap(key):
    # F-keys use the evdev codes in the installed XKB pc/us layout. Unlike
    # wtype's dynamically assigned code1, these also match Hyprland's default
    # resolve_binds_by_sym=false translation. No compositor config is changed.
    import ctypes as c
    lib = c.CDLL('libxkbcommon.so.0')
    lib.xkb_context_new.argtypes, lib.xkb_context_new.restype = [c.c_int], c.c_void_p
    lib.xkb_context_unref.argtypes = [c.c_void_p]
    lib.xkb_keymap_new_from_string.argtypes = [c.c_void_p, c.c_char_p, c.c_int, c.c_int]
    lib.xkb_keymap_new_from_string.restype = c.c_void_p
    lib.xkb_keymap_unref.argtypes = [c.c_void_p]
    lib.xkb_keymap_get_as_string.argtypes = [c.c_void_p, c.c_int]
    lib.xkb_keymap_get_as_string.restype = c.c_void_p
    lib.xkb_keymap_mod_get_index.argtypes = [c.c_void_p, c.c_char_p]
    lib.xkb_keymap_mod_get_index.restype = c.c_uint
    lib.xkb_keymap_key_get_syms_by_level.argtypes = [c.c_void_p, c.c_uint, c.c_uint, c.c_uint, c.POINTER(c.POINTER(c.c_uint))]
    lib.xkb_keymap_key_get_syms_by_level.restype = c.c_int
    source = b'xkb_keymap { xkb_keycodes { include "evdev+aliases(qwerty)" }; xkb_types { include "complete" }; xkb_compatibility { include "complete" }; xkb_symbols { include "pc+us+inet(evdev)" }; };'
    context = lib.xkb_context_new(0)
    keymap = None
    allocation = None
    try:
        if not context:
            raise RuntimeError('test_xkb_context_failed')
        keymap = lib.xkb_keymap_new_from_string(context, source, 1, 0)
        if not keymap:
            raise RuntimeError('test_xkb_keymap_failed')
        code = {'F12': 88, 'F11': 87, 'F10': 68, 'F8': 66, 'V': 47}[key]
        symbols = c.POINTER(c.c_uint)()
        count = lib.xkb_keymap_key_get_syms_by_level(keymap, code+8, 0, 0, c.byref(symbols))
        if count != 1 or symbols[0] != (0x76 if key == 'V' else 0xffbd + int(key[1:])):
            raise RuntimeError('test_xkb_function_key_mismatch')
        mask = 0
        for name in ((b'Control',) if key == 'V' else (b'Shift', b'Control', b'Mod4')):
            index = lib.xkb_keymap_mod_get_index(keymap, name)
            if index >= 32:
                raise RuntimeError('test_xkb_modifier_missing')
            mask |= 1 << index
        if mask != (4 if key == 'V' else 69):
            raise RuntimeError('test_xkb_modifier_mask_unexpected')
        allocation = lib.xkb_keymap_get_as_string(keymap, 1)
        if not allocation:
            raise RuntimeError('test_xkb_serialization_failed')
        return c.string_at(allocation) + b'\0', code, mask
    finally:
        if allocation:
            libc = c.CDLL(None)
            libc.free.argtypes = [c.c_void_p]
            libc.free(allocation)
        if keymap:
            lib.xkb_keymap_unref(keymap)
        if context:
            lib.xkb_context_unref(context)


def release_keyboard(key, target_pid, target_address):
    # Minimal client for the installed virtual-keyboard-v1 protocol. A separate
    # socket owns all objects; disconnect is the final cleanup even on errors.
    import array
    import struct
    keymap, code, mask = release_keymap(key)
    display = os.environ['WAYLAND_DISPLAY']
    path = display if os.path.isabs(display) else str(Path(os.environ['XDG_RUNTIME_DIR']) / display)
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(2)
        connection.connect(path)
        pending = bytearray()
        globals_found = {}
        next_id = 3
        def message(object_id, opcode, payload=b'', fd=None):
            data = struct.pack('=II', object_id, (len(payload)+8) << 16 | opcode) + payload
            if fd is None:
                connection.sendall(data)
            else:
                sent = connection.sendmsg([data], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array('i', [fd]))])
                if sent < len(data):
                    connection.sendall(data[sent:])
        def string(value):
            data = value.encode() + b'\0'
            return struct.pack('=I', len(data)) + data + b'\0' * (-len(data) % 4)
        def roundtrip():
            nonlocal next_id
            callback = next_id
            next_id += 1
            message(1, 0, struct.pack('=I', callback))
            deadline = time.monotonic()+2
            while True:
                if len(pending) < 8:
                    size = None
                else:
                    object_id, word = struct.unpack_from('=II', pending)
                    size, opcode = word >> 16, word & 0xffff
                    if size < 8 or size > 65535:
                        raise RuntimeError('test_wayland_invalid_frame')
                if size is None or len(pending) < size:
                    if time.monotonic() >= deadline:
                        raise TimeoutError('test_wayland_roundtrip_timeout')
                    connection.settimeout(max(0.01, deadline-time.monotonic()))
                    data = connection.recv(65536)
                    if not data:
                        raise RuntimeError('test_wayland_eof')
                    pending.extend(data)
                    if len(pending) > LIMIT:
                        raise RuntimeError('test_wayland_buffer_limit')
                    continue
                payload = bytes(pending[8:size])
                del pending[:size]
                if object_id == 1 and opcode == 0:
                    raise RuntimeError('test_wayland_protocol_error')
                if object_id == 2 and opcode == 0:
                    name, length = struct.unpack_from('=II', payload)
                    interface = payload[8:8+length-1].decode()
                    version = struct.unpack_from('=I', payload, 8+((length+3)//4)*4)[0]
                    globals_found.setdefault(interface, []).append((name, version))
                if object_id == callback and opcode == 0:
                    return
        message(1, 1, struct.pack('=I', 2))  # wl_display.get_registry
        roundtrip()
        next_id = 7  # IDs 4/5/6 below, after registry2 and initial sync3.
        for interface, object_id in (('zwp_virtual_keyboard_manager_v1', 4), ('wl_seat', 5)):
            options = globals_found.get(interface, [])
            if len(options) != 1:
                raise RuntimeError('test_wayland_missing_or_ambiguous_' + interface)
            message(2, 0, struct.pack('=I', options[0][0]) + string(interface) + struct.pack('=II', 1, object_id))
        message(4, 0, struct.pack('=II', 5, 6))
        fd = os.memfd_create('voice-input-m0-keymap', os.MFD_CLOEXEC)
        try:
            os.write(fd, keymap)
            message(6, 0, struct.pack('=II', 1, len(keymap)), fd=fd)
            roundtrip()
        finally:
            os.close(fd)
        target_focus(target_pid, target_address)
        pressed = False
        try:
            message(6, 2, struct.pack('=IIII', mask, 0, 0, 0))
            message(6, 1, struct.pack('=III', int(time.monotonic()*1000) & 0xffffffff, code, 1))
            pressed = True
            roundtrip()
            if key != 'V':
                print(json.dumps({'event': 'held', 'evdev_code': code, 'modifier_mask': mask}), flush=True)
                time.sleep(1)
        finally:
            if pressed:
                message(6, 1, struct.pack('=III', int(time.monotonic()*1000) & 0xffffffff, code, 0))
            message(6, 2, struct.pack('=IIII', 0, 0, 0, 0))
            message(6, 3)
            roundtrip()
        if key == 'V':
            print(json.dumps({'event': 'injected', 'evdev_code': code, 'modifier_mask': mask}), flush=True)
    return 0


def gtk_target(control=False):
    import gi
    gi.require_version('Gtk', '4.0')
    from gi.repository import Gtk, GLib
    app = Gtk.Application(application_id=None)
    expected = hashlib.sha256(TEXT.encode()).hexdigest()

    def activate(application):
        win = Gtk.ApplicationWindow(application=application)
        win.set_title('voice-input-m0-owned-test')
        win.set_default_size(540, 240)
        view = Gtk.TextView()
        view.set_wrap_mode(Gtk.WrapMode.WORD_CHAR)
        win.set_child(view)
        buffer = view.get_buffer()

        def changed(buf):
            start, end = buf.get_bounds()
            value = buf.get_text(start, end, True).encode()
            print(json.dumps({'event': 'buffer', 'matches': hashlib.sha256(value).hexdigest() == expected,
                              'bytes': len(value)}), flush=True)
        buffer.connect('changed', changed)
        if control:
            def reset_input(source, condition):
                if condition & (GLib.IO_HUP | GLib.IO_ERR):
                    return False
                if source.readline() != '{"op":"reset"}\n':
                    return False
                buffer.set_text('')
                print('{"event":"buffer_reset"}', flush=True)
                return True
            GLib.io_add_watch(sys.stdin, GLib.PRIORITY_DEFAULT, GLib.IO_IN | GLib.IO_HUP | GLib.IO_ERR, reset_input)
        win.present()
        view.grab_focus()
        GLib.timeout_add(150, lambda: (print('{"event":"ready"}', flush=True), False)[1])
    app.connect('activate', activate)
    return app.run([])


def clipboard_restore():
    # Input is private pipe data; never put clipboard contents in argv or files.
    payload = json.loads(sys.stdin.readline())
    import gi
    gi.require_version('Gtk', '4.0')
    from gi.repository import Gdk, GLib
    display = Gdk.Display.open(None)
    if display is None:
        raise RuntimeError('restore_display_unavailable')
    clipboard = display.get_clipboard()
    providers = [Gdk.ContentProvider.new_for_bytes(mime, GLib.Bytes.new(base64.b64decode(value)))
                 for mime, value in payload.items()]
    provider = Gdk.ContentProvider.new_union(providers)
    if not clipboard.set_content(provider):
        raise RuntimeError('restore_ownership_failed')
    print('{"event":"restored"}', flush=True)
    loop = GLib.MainLoop()
    def check():
        if not clipboard.is_local():
            loop.quit()
            return False
        return True
    GLib.timeout_add(500, check)
    loop.run()
    return 0


def capture_exact_audio():
    args = ['pw-record', '--raw', '--rate', '16000', '--channels', '1', '--format', 's16', '-']
    proc = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    pcm = bytearray()
    deadline = time.monotonic() + 4
    try:
        while len(pcm) < 16000:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([proc.stdout], [], [], remaining)[0]:
                raise TimeoutError('microphone_capture_timeout')
            block = os.read(proc.stdout.fileno(), 16000 - len(pcm))
            if not block:
                raise RuntimeError('microphone_capture_early_eof')
            pcm.extend(block)
        return {'status': 'pass', 'pcm_bytes_in_memory': len(pcm), 'saved_audio_files': 0,
                'termination': 'probe explicitly stops recorder after exact 8000 samples; exit status not used as success',
                'scope': 'default source 0.5 second transport only; no speech quality claim'}
    finally:
        pcm.clear()
        if proc.poll() is None:
            proc.terminate()
        try:
            proc.wait(timeout=1)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        proc.stdout.close()


def focus_address(address):
    # Exact syntax from installed omarchy-hyprland-focus-app:33, Hyprland 0.57.
    command(['hyprctl', 'dispatch', 'hl.dsp.focus({ window = ' + json.dumps('address:' + address) + ' })'])
    deadline = time.monotonic() + 1
    while time.monotonic() < deadline:
        if hypr_json('activewindow').get('address') == address:
            return
        time.sleep(0.025)
    raise RuntimeError('owned_window_focus_not_observed_within_1s')


def hypr_json(name):
    return json.loads(command(['hyprctl', '-j', name]))


def target_focus(pid, address):
    value = hypr_json('activewindow')
    if value.get('pid') != pid or value.get('address') != address:
        raise RuntimeError('focus_guard_refused_nonowned_target:pid_match=' + str(value.get('pid') == pid) + ':address_match=' + str(value.get('address') == address))


_reply_buffers = {}


def wait_json(proc, predicate, timeout=5):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise RuntimeError('owned_target_exited')
        pending = _reply_buffers.get(proc.pid, b'')
        if b'\n' in pending:
            line, pending = pending.split(b'\n', 1)
            _reply_buffers[proc.pid] = pending
            item = json.loads(line)
            if predicate(item):
                return item
            continue
        if select.select([proc.stdout], [], [], 0.1)[0]:
            block = os.read(proc.stdout.fileno(), 65536)
            if not block:
                raise RuntimeError('owned_target_pipe_closed')
            _reply_buffers[proc.pid] = pending + block
    raise TimeoutError('owned_target_reply_timeout')


def x11_physical_diagnostic(proc, address):
    # Separate diagnostic: never changes the result of the original wtype path.
    # Reset our own editor over a private control pipe, so even an incorrect
    # character from the first injector cannot contaminate the second check.
    try:
        target_focus(proc.pid, address)
        proc.stdin.write('{"op":"reset"}\n')
        proc.stdin.flush()
        wait_json(proc, lambda item: item.get('event') == 'buffer_reset', 1)
        target_focus(proc.pid, address)
        injected = json.loads(command([sys.executable, str(Path(__file__).resolve()),
            '--paste-physical', '--owned-pid', str(proc.pid), '--owned-address', address], 3))
        if injected != {'event': 'injected', 'evdev_code': 47, 'modifier_mask': 4}:
            raise RuntimeError('physical_paste_injector_reply_invalid')
        wait_json(proc, lambda item: item.get('event') == 'buffer' and item.get('matches'), 3)
        return {'status': 'pass', 'matches': True, 'xwayland': True, 'evdev_code': 47,
                'modifier_mask': 4, 'injection': 'separate synthetic physical-layout Ctrl+V diagnostic',
                'scope': 'owned target only; not original wtype compatibility or product/V1 acceptance'}
    except Exception as error:
        return {'status': 'blocked', 'reason': str(error),
                'scope': 'separate physical-keycode diagnostic; original wtype result unchanged'}


def owner(text):
    p = subprocess.Popen(['wl-copy', '--foreground', '--type', 'text/plain;charset=utf-8'],
                         stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    p.stdin.write(text)
    p.stdin.close()
    return p


def run_probe():
    report = {'captured_at_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
              'scope': 'M0 real desktop capabilities; not V01-V11 product acceptance',
              'synthetic_text_sha256': hashlib.sha256(TEXT.encode()).hexdigest(),
              'checks': {}, 'cleanup': {}, 'versions': {}}
    checks = report['checks']
    try:
        locked_result = hypr_json('locked')
        compositor_locked = locked_result.get('locked')
        if not isinstance(compositor_locked, bool):
            raise RuntimeError('invalid_compositor_lock_response')
        report['compositor_locked'] = compositor_locked
    except Exception as error:
        compositor_locked = None
        report['compositor_locked'] = None
        checks['compositor_lock_query'] = {'status': 'blocked', 'reason': str(error)}
    for name, args in {
        'hyprland': ['hyprctl', 'version'],
        'packages': ['pacman', '-Q', 'hyprland', 'omarchy', 'wl-clipboard', 'wtype', 'gtk4', 'pipewire'],
        'python': ['python3', '--version'],
    }.items():
        try:
            report['versions'][name] = command(args, 3).decode().strip()
        except Exception as error:
            report['versions'][name] = str(error)
    targets, owners, restore_proc = [], [], None
    clipboard_saved, clipboard_touched = None, False
    temporary_binding = None
    binding_installed = False
    event_socket = None
    old_window = None
    with tempfile.TemporaryDirectory(prefix='voice-input-m0-') as temp:
        marker = Path(temp) / 'release-marker'
        try:
            try:
                old_window = hypr_json('activewindow').get('address')
                events_path = Path(os.environ['XDG_RUNTIME_DIR']) / 'hypr' / os.environ['HYPRLAND_INSTANCE_SIGNATURE'] / '.socket2.sock'
                event_socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                event_socket.settimeout(0.5)
                event_socket.connect(str(events_path))
                event_socket.setblocking(False)
                checks['socket2_connect'] = {'status': 'pass'}
            except Exception as error:
                checks['socket2_connect'] = {'status': 'blocked', 'reason': str(error)}
            try:
                errors = command(['hyprctl', 'configerrors']).decode().strip()
                checks['config_errors'] = {'status': 'pass' if not errors else 'fail', 'present': bool(errors)}
            except Exception as error:
                checks['config_errors'] = {'status': 'blocked', 'reason': str(error)}
            try:
                bindings = hypr_json('binds')
                checks['reference_key_conflict'] = {'status': 'observed', 'super_shift_d_count': sum(
                    str(b.get('key', '')).lower() == 'd' and b.get('modmask') == 65 for b in bindings)}
                for key in ('F12', 'F11', 'F10', 'F8'):
                    if not any(str(b.get('key', '')).lower() == key.lower() and b.get('modmask') == 69 for b in bindings):
                        temporary_binding = 'SUPER + CTRL + SHIFT + ' + key
                        break
                if not temporary_binding:
                    raise RuntimeError('no_empty_probe_key')
                checks['available_key'] = {'status': 'pass', 'key': temporary_binding}
            except Exception as error:
                checks['available_key'] = {'status': 'blocked', 'reason': str(error)}
            try:
                types = command(['wl-paste', '--list-types']).decode().splitlines()
                if len(types) > 32:
                    raise RuntimeError('too_many_clipboard_formats')
                clipboard_saved = {}
                total = 0
                for mime in types:
                    value = read_bounded(['wl-paste', '--no-newline', '--type', mime])
                    total += len(value)
                    if total > LIMIT:
                        raise RuntimeError('clipboard_snapshot_too_large')
                    clipboard_saved[mime] = value
                checks['clipboard_snapshot'] = {'status': 'pass', 'mime_count': len(types)}
            except Exception as error:
                # An empty clipboard is safe only when explicitly reported empty.
                if 'Nothing is copied' in str(error):
                    clipboard_saved = {}
                    checks['clipboard_snapshot'] = {'status': 'pass', 'mime_count': 0}
                else:
                    clipboard_saved = None
                    checks['clipboard_snapshot'] = {'status': 'blocked', 'reason': str(error)}
            for backend in ('wayland', 'x11'):
                phase = 'preconditions'
                name = 'controlled_paste_' + backend
                try:
                    if compositor_locked is not False:
                        raise RuntimeError('compositor_locked' if compositor_locked else 'compositor_lock_unknown')
                    if clipboard_saved is None:
                        raise RuntimeError('clipboard_backup_unavailable_no_write')
                    phase = 'target_startup'
                    env = dict(os.environ, GDK_BACKEND=backend)
                    target_errors = tempfile.TemporaryFile()
                    proc = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), '--target', '--target-control'],
                                            env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=target_errors,
                                            text=True, bufsize=1)
                    targets.append(proc)
                    wait_json(proc, lambda item: item.get('event') == 'ready')
                    phase = 'target_focus'
                    matching = [w for w in hypr_json('clients') if w.get('pid') == proc.pid]
                    if len(matching) != 1:
                        raise RuntimeError('owned_target_not_unique')
                    window = matching[0]
                    address = window['address']
                    is_x11 = bool(window.get('xwayland'))
                    if is_x11 != (backend == 'x11'):
                        raise RuntimeError('unexpected_backend')
                    focus_address(address)
                    target_focus(proc.pid, address)
                    phase = 'clipboard_roundtrip'
                    first = owner(TEXT.encode())
                    owners.append(first)
                    clipboard_touched = True
                    deadline = time.monotonic() + 2
                    while time.monotonic() < deadline:
                        try:
                            if read_bounded(['wl-paste', '--no-newline']) == TEXT.encode():
                                break
                        except Exception:
                            pass
                        time.sleep(0.03)
                    else:
                        raise RuntimeError('clipboard_roundtrip_mismatch')
                    owner_alive = first.poll() is None
                    target_focus(proc.pid, address)
                    phase = 'paste_and_buffer_reply'
                    started = time.monotonic()
                    command(['wtype', '-M', 'ctrl', '-k', 'v', '-m', 'ctrl'])
                    inject_ms = round((time.monotonic() - started)*1000, 2)
                    try:
                        wait_json(proc, lambda item: item.get('event') == 'buffer' and item.get('matches'), 3)
                    finally:
                        if backend == 'x11':
                            checks['controlled_paste_x11_physical_keycode'] = x11_physical_diagnostic(proc, address)
                    second = owner(b'voice-input-m0-owner-replacement')
                    owners.append(second)
                    try:
                        first.wait(timeout=1)
                        replaced_exited = True
                    except subprocess.TimeoutExpired:
                        replaced_exited = False
                    checks[name] = {'status': 'pass', 'matches': True, 'xwayland': is_x11,
                                    'owner_alive_before_replacement': owner_alive,
                                    'old_owner_exited_after_replacement': replaced_exited,
                                    'wtype_command_ms': inject_ms}
                    if not owner_alive or not replaced_exited:
                        checks[name]['status'] = 'partial'
                    if backend == 'wayland' and temporary_binding:
                        try:
                            bind_command = shlex.join([sys.executable, str(Path(__file__).resolve()), '--mark', str(marker)])
                            lua = f'hl.bind({json.dumps(temporary_binding)}, hl.dsp.exec_cmd({json.dumps(bind_command)}), {{release=true, description="voice-input-m0-temporary"}})'
                            command(['hyprctl', 'eval', lua])
                            binding_installed = True
                            observed = [b for b in hypr_json('binds') if b.get('description') == 'voice-input-m0-temporary']
                            if len(observed) != 1 or not observed[0].get('release'):
                                raise RuntimeError('release_binding_not_registered')
                            target_focus(proc.pid, address)
                            key = temporary_binding.rsplit(' ', 1)[1]
                            keyproc = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), '--release-key', key,
                                '--owned-pid', str(proc.pid), '--owned-address', address],
                                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
                            try:
                                held = wait_json(keyproc, lambda item: item.get('event') == 'held', 3)
                                time.sleep(0.4)
                                early = marker.exists()
                                if keyproc.wait(timeout=3):
                                    raise RuntimeError('release_injector_failed')
                            finally:
                                if keyproc.poll() is None:
                                    keyproc.kill()
                                    keyproc.wait()
                                keyproc.stdout.close()
                            deadline = time.monotonic() + 2
                            while not marker.exists() and time.monotonic() < deadline:
                                time.sleep(0.03)
                            count = len(marker.read_text().splitlines()) if marker.exists() else 0
                            checks['release_binding'] = {'status': 'pass' if not early and count == 1 else 'fail',
                                'registered_release': True, 'fired_while_held': early, 'release_callback_count': count,
                                'key': temporary_binding, 'evdev_code': held['evdev_code'],
                                'modifier_mask': held['modifier_mask'], 'injection': 'real compositor with synthetic physical-layout virtual keyboard; not a human keypress'}
                        except Exception as error:
                            checks['release_binding'] = {'status': 'blocked', 'reason': str(error)}
                        finally:
                            command(['hyprctl', 'eval', f'hl.unbind({json.dumps(temporary_binding)})'])
                            report['cleanup']['temporary_binding_removed'] = not any(
                                b.get('description') == 'voice-input-m0-temporary' for b in hypr_json('binds'))
                            temporary_binding = None
                            binding_installed = False
                except Exception as error:
                    target_errors.seek(0) if 'target_errors' in locals() else None
                    diagnostic = target_errors.read(2048).decode(errors='replace') if 'target_errors' in locals() else ''
                    checks[name] = {'status': 'blocked', 'reason': str(error), 'phase': locals().get('phase', 'preconditions'), 'owned_target_diagnostic': diagnostic}
                finally:
                    if 'target_errors' in locals():
                        target_errors.close()
                        del target_errors
            # Observe controlled target close events as well as focus/open events.
            for proc in targets:
                if proc.poll() is None:
                    proc.terminate()
                try:
                    proc.wait(timeout=1)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
            if event_socket:
                event_names, carry = set(), b''
                event_deadline = time.monotonic() + 1
                while time.monotonic() < event_deadline and select.select([event_socket], [], [], 0.15)[0]:
                    block = event_socket.recv(65536)
                    if not block:
                        break
                    carry += block
                    lines = carry.split(b'\n')
                    carry = lines.pop()
                    for line in lines:
                        name = line.split(b'>>', 1)[0].decode(errors='replace')
                        if name in ('activewindow', 'activewindowv2', 'openwindow', 'closewindow'):
                            event_names.add(name)
                checks['socket2_events'] = {'status': 'pass' if {'activewindowv2', 'closewindow'} <= event_names else 'partial',
                                            'event_names_only': sorted(event_names)}
            try:
                checks['microphone_capture'] = capture_exact_audio()
            except Exception as error:
                checks['microphone_capture'] = {'status': 'blocked', 'reason': str(error)}
        finally:
            if temporary_binding and binding_installed:
                try:
                    command(['hyprctl', 'eval', f'hl.unbind({json.dumps(temporary_binding)})'])
                    report['cleanup']['temporary_binding_removed'] = True
                except Exception as error:
                    report['cleanup']['binding_error'] = str(error)
            if clipboard_touched and clipboard_saved is not None:
                try:
                    if clipboard_saved:
                        restore_proc = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), '--restore'],
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                            text=True, start_new_session=True)
                        restore_proc.stdin.write(json.dumps({k: base64.b64encode(v).decode() for k,v in clipboard_saved.items()})+'\n')
                        restore_proc.stdin.close()
                        wait_json(restore_proc, lambda item: item.get('event') == 'restored', 5)
                        equal = all(read_bounded(['wl-paste', '--no-newline', '--type', mime]) == value
                                    for mime, value in clipboard_saved.items())
                        report['cleanup']['clipboard_restored_all_mime_bytes'] = equal
                        report['cleanup']['restore_owner_lifetime'] = 'until next clipboard ownership change'
                    else:
                        command(['wl-copy', '--clear'])
                        report['cleanup']['empty_clipboard_restored'] = True
                except Exception as error:
                    report['cleanup']['clipboard_restore_error'] = str(error)
            for proc in targets + owners:
                if proc.poll() is None:
                    proc.terminate()
                try:
                    proc.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
            report['cleanup']['owned_targets_closed'] = all(p.poll() is not None for p in targets)
            if old_window:
                try:
                    focus_address(old_window)
                    report['cleanup']['original_window_focus_requested'] = True
                except Exception:
                    report['cleanup']['original_window_focus_requested'] = False
            if event_socket:
                event_socket.close()
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--owned-pid', type=int, help=argparse.SUPPRESS)
    parser.add_argument('--owned-address', help=argparse.SUPPRESS)
    parser.add_argument('--release-key', choices=('F12', 'F11', 'F10', 'F8'), help=argparse.SUPPRESS)
    parser.add_argument('--paste-physical', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--target-control', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--target', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--restore', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--mark', type=Path, help=argparse.SUPPRESS)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    if args.release_key or args.paste_physical:
        if not args.owned_pid or not args.owned_address:
            parser.error('release helper requires an owned target PID and address')
        return release_keyboard('V' if args.paste_physical else args.release_key, args.owned_pid, args.owned_address)
    if args.target:
        return gtk_target(args.target_control)
    if args.restore:
        return clipboard_restore()
    if args.mark:
        with args.mark.open('a') as output:
            output.write(str(time.monotonic_ns())+'\n')
        return 0
    if args.output and args.output.exists():
        parser.error('output exists; choose a new evidence file')
    report = run_probe()
    if args.output:
        with args.output.open('x') as output:
            json.dump(report, output, ensure_ascii=False, indent=2)
            output.write('\n')
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if all(v.get('status') in ('pass', 'observed') for v in report['checks'].values()) else 2


if __name__ == '__main__':
    raise SystemExit(main())
