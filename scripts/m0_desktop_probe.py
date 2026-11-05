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


def gtk_target():
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
                name = 'controlled_paste_' + backend
                try:
                    if compositor_locked is not False:
                        raise RuntimeError('compositor_locked' if compositor_locked else 'compositor_lock_unknown')
                    if clipboard_saved is None:
                        raise RuntimeError('clipboard_backup_unavailable_no_write')
                    env = dict(os.environ, GDK_BACKEND=backend)
                    proc = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), '--target'],
                                            env=env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                            text=True, bufsize=1)
                    targets.append(proc)
                    wait_json(proc, lambda item: item.get('event') == 'ready')
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
                    started = time.monotonic()
                    command(['wtype', '-M', 'ctrl', '-k', 'v', '-m', 'ctrl'])
                    inject_ms = round((time.monotonic() - started)*1000, 2)
                    wait_json(proc, lambda item: item.get('event') == 'buffer' and item.get('matches'), 3)
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
                            keyproc = subprocess.Popen(['wtype', '-M', 'logo', '-M', 'ctrl', '-M', 'shift',
                                '-P', key, '-s', '1000', '-p', key, '-m', 'shift', '-m', 'ctrl', '-m', 'logo'],
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                            time.sleep(0.4)
                            early = marker.exists()
                            keyproc.wait(timeout=3)
                            deadline = time.monotonic() + 2
                            while not marker.exists() and time.monotonic() < deadline:
                                time.sleep(0.03)
                            count = len(marker.read_text().splitlines()) if marker.exists() else 0
                            checks['release_binding'] = {'status': 'pass' if not early and count == 1 else 'fail',
                                'registered_release': True, 'fired_while_held': early, 'release_callback_count': count,
                                'key': temporary_binding, 'injection': 'real compositor with virtual keyboard'}
                        except Exception as error:
                            checks['release_binding'] = {'status': 'blocked', 'reason': str(error)}
                        finally:
                            command(['hyprctl', 'eval', f'hl.unbind({json.dumps(temporary_binding)})'])
                            report['cleanup']['temporary_binding_removed'] = not any(
                                b.get('description') == 'voice-input-m0-temporary' for b in hypr_json('binds'))
                            temporary_binding = None
                            binding_installed = False
                except Exception as error:
                    checks[name] = {'status': 'blocked', 'reason': str(error)}
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
    parser.add_argument('--target', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--restore', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--mark', type=Path, help=argparse.SUPPRESS)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    if args.target:
        return gtk_target()
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
