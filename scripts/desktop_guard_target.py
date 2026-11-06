#!/usr/bin/env python3
"""Two owned empty GTK4 targets; fixed stdin commands only, no input injection."""
import argparse
import base64
import json
import os
from pathlib import Path
import sys
import subprocess
import time

import gi
gi.require_version('Gtk', '4.0')
from gi.repository import GLib, Gtk
import m0_desktop_probe as desktop

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--backend', choices=('wayland', 'x11'), required=True)
backend = parser.parse_args().backend
expected_xwayland = backend == 'x11'
PROBE_TEXT = '语音输入粘贴验证 2026\nVoice input paste probe: /tmp/example.txt'
saved_clipboard = None
clipboard_restored = False
original = desktop.hypr_json('activewindow')
original = {key: original.get(key) for key in ('address', 'pid')}
def start_ticks(pid):
    try:
        return Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[19]
    except (OSError, IndexError):
        return None
original_ticks = start_ticks(original['pid']) if original['pid'] else None
app = Gtk.Application(application_id=None)
windows = {}
addresses = {}
buffers = {}
changes = {'a': 0, 'b': 0}

def reply(value):
    print(json.dumps(value), flush=True)

def own_clients():
    return [item for item in desktop.hypr_json('clients') if item.get('pid') == os.getpid()]

def validate(name):
    match = [item for item in own_clients() if item.get('address') == addresses[name]]
    if len(match) != 1 or bool(match[0].get('xwayland')) != expected_xwayland:
        raise RuntimeError('owned_backend_identity_changed')

def snapshot_clipboard():
    global saved_clipboard
    if saved_clipboard is not None:
        raise RuntimeError('clipboard_already_snapshotted')
    try:
        types = desktop.command(['wl-paste', '--list-types']).decode().splitlines()
    except Exception as error:
        if 'Nothing is copied' not in str(error):
            raise
        types = []
    if len(types) > 32:
        raise RuntimeError('too_many_clipboard_formats')
    saved = {}
    for mime in types:
        saved[mime] = desktop.read_bounded(['wl-paste', '--no-newline', '--type', mime])
        if sum(map(len, saved.values())) > desktop.LIMIT:
            raise RuntimeError('clipboard_snapshot_too_large')
    saved_clipboard = saved

def restore_clipboard():
    global clipboard_restored
    if saved_clipboard is None:
        return True
    if saved_clipboard:
        owner = subprocess.Popen([sys.executable, str(Path(desktop.__file__).resolve()), '--restore'],
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 stderr=subprocess.DEVNULL, text=True, start_new_session=True,
                                 env={**os.environ, 'GDK_BACKEND': 'wayland'})
        owner.stdin.write(json.dumps({mime: base64.b64encode(value).decode()
                                      for mime, value in saved_clipboard.items()})+'\n')
        owner.stdin.close()
        desktop.wait_json(owner, lambda item: item.get('event') == 'restored', 5)
        clipboard_restored = all(desktop.read_bounded(['wl-paste', '--no-newline', '--type', mime]) == value
                                 for mime, value in saved_clipboard.items())
    else:
        desktop.command(['wl-copy', '--clear'])
        clipboard_restored = True
    return clipboard_restored

def restore_original():
    active = desktop.hypr_json('activewindow')
    # Do not override a user's intervening focus choice.
    if active.get('pid') != os.getpid():
        return False
    if not original_ticks or start_ticks(original['pid']) != original_ticks:
        return False
    matches = [item for item in desktop.hypr_json('clients')
               if item.get('address') == original['address'] and item.get('pid') == original['pid']]
    if len(matches) != 1:
        return False
    desktop.focus_address(original['address'])
    return True

def control(source, condition):
    if condition & (GLib.IO_HUP | GLib.IO_ERR):
        app.quit()
        return False
    try:
        line = source.readline(1025)
        if not line or len(line) > 1024:
            raise RuntimeError('invalid_request')
        request = json.loads(line)
        if set(request) - {'op', 'target'}:
            raise RuntimeError('invalid_fields')
        op = request['op']
        if op == 'focus' and request.get('target') in ('a', 'b'):
            name = request['target']
            validate(name)
            desktop.focus_address(addresses[name])
            desktop.target_focus(os.getpid(), addresses[name])
            reply({'event': 'focused', 'target': name, 'pid': os.getpid(), 'address': addresses[name]})
        elif op == 'verify_focus' and request.get('target') in ('a', 'b'):
            name = request['target']
            validate(name)
            desktop.target_focus(os.getpid(), addresses[name])
            reply({'event': 'focus_verified', 'target': name})
        elif op == 'close_a':
            validate('a')
            windows['a'].close()
            reply({'event': 'close_requested'})
        elif op == 'status':
            reply({'event': 'status', 'changes': changes,
                   'bytes': {name: len(buf.get_text(*buf.get_bounds(), True).encode())
                             for name, buf in buffers.items()},
                   'a_present': any(item.get('address') == addresses['a'] for item in own_clients())})
        elif op == 'prepare_positive':
            validate('b')
            desktop.target_focus(os.getpid(), addresses['b'])
            snapshot_clipboard()
            reply({'event': 'positive_prepared', 'clipboard_mime_count': len(saved_clipboard)})
        elif op == 'positive_status':
            # Read the actual GTK buffer; never assign expected text or accept
            # arbitrary input text from the test driver.
            buffer = buffers['b']
            text = buffer.get_text(*buffer.get_bounds(), True)
            reply({'event': 'positive_status', 'matches': text == PROBE_TEXT,
                   'changes': changes['b'], 'bytes': len(text.encode())})
        elif op == 'cleanup':
            restored_clipboard = restore_clipboard()
            restored = restore_original()
            reply({'event': 'cleanup', 'original_focus_restored': restored,
                   'clipboard_restored': restored_clipboard,
                   'restore_owner_lifetime': 'until next ownership change' if saved_clipboard else 'none; original clipboard empty',
                   'restored_clipboard_mime_count': len(saved_clipboard) if saved_clipboard is not None else 0})
            app.quit()
            return False
        else:
            raise RuntimeError('invalid_operation')
    except Exception as error:
        reply({'event': 'error', 'code': type(error).__name__})
        app.quit()
        return False
    return True

def activate(application):
    for name in ('a', 'b'):
        window = Gtk.ApplicationWindow(application=application)
        window.set_title('voice-input-guard-owned-'+name)
        window.set_default_size(450, 180)
        view = Gtk.TextView()
        view.set_wrap_mode(Gtk.WrapMode.WORD_CHAR)
        window.set_child(view)
        buffers[name] = view.get_buffer()
        buffers[name].connect('changed', lambda _buffer, key=name: changes.__setitem__(key, changes[key]+1))
        windows[name] = window
        window.present()
        view.grab_focus()
    deadline = time.monotonic()+3
    def ready():
        for item in own_clients():
            for name in ('a', 'b'):
                if (item.get('title') == 'voice-input-guard-owned-'+name
                        and bool(item.get('xwayland')) == expected_xwayland):
                    addresses[name] = item['address']
        if len(addresses) != 2:
            if time.monotonic() >= deadline:
                reply({'event': 'error', 'code': 'owned_windows_not_ready'})
                app.quit()
                return False
            return True
        reply({'event': 'ready', 'pid': os.getpid(), 'addresses': addresses,
               'backend': backend, 'native_xwayland': expected_xwayland})
        GLib.io_add_watch(sys.stdin, GLib.PRIORITY_DEFAULT, GLib.IO_IN | GLib.IO_HUP | GLib.IO_ERR, control)
        return False
    GLib.timeout_add(50, ready)

app.connect('activate', activate)
try:
    result = app.run([])
finally:
    if saved_clipboard is not None and not clipboard_restored:
        restore_clipboard()
raise SystemExit(result)
