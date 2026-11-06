#!/usr/bin/env python3
"""Run the explicit debug own-window paste test, restoring clipboard MIME bytes."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

import m0_desktop_probe as desktop


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.output.exists():
        parser.error('output exists; choose a new evidence file')
    root = Path(__file__).resolve().parents[1]
    binary = root / 'target/debug/voice-input'
    if desktop.hypr_json('locked').get('locked') is not False:
        parser.error('an unlocked desktop is required')
    saved = {}
    try:
        types = desktop.command(['wl-paste', '--list-types']).decode().splitlines()
    except Exception as error:
        if 'Nothing is copied' not in str(error):
            raise
        types = []
    if len(types) > 32:
        parser.error('clipboard snapshot exceeds MIME limit; no paste attempted')
    for mime in types:
        saved[mime] = desktop.read_bounded(['wl-paste', '--no-newline', '--type', mime])
        if sum(map(len, saved.values())) > desktop.LIMIT:
            parser.error('clipboard snapshot exceeds byte limit; no paste attempted')
    report = {
        'scope': 'Actual debug Tauri own-window native paste with trusted DOM events and production typed commands; no microphone or other-window injection',
        'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
        'clipboard_snapshot_mime_count': len(saved),
        'passed': False,
    }
    try:
        with tempfile.TemporaryDirectory(prefix='voice-own-paste-') as temporary:
            env = {**os.environ, 'XDG_DATA_HOME': temporary+'/data',
                   'XDG_CONFIG_HOME': temporary+'/config', 'XDG_CACHE_HOME': temporary+'/cache'}
            process = subprocess.Popen([str(binary), '--delivery-smoke'], env=env,
                                       stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                       stderr=subprocess.PIPE, text=True)
            try:
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline and process.poll() is None:
                    matches = [window for window in desktop.hypr_json('clients')
                               if window.get('pid') == process.pid]
                    if len(matches) == 1:
                        report['owned_window_xwayland'] = bool(matches[0].get('xwayland'))
                        break
                    time.sleep(.05)
                stdout, stderr = process.communicate(timeout=25)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)
            report.update(returncode=process.returncode, stderr=stderr)
            report['ui_report'] = json.loads(stdout.strip())
            report['passed'] = process.returncode == 0 and report['ui_report']['passed']
    finally:
        try:
            if saved:
                owner = subprocess.Popen([sys.executable, str(root/'scripts/m0_desktop_probe.py'), '--restore'],
                                         stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                         stderr=subprocess.DEVNULL, text=True, start_new_session=True)
                owner.stdin.write(json.dumps({mime: base64.b64encode(value).decode()
                                              for mime, value in saved.items()})+'\n')
                owner.stdin.close()
                desktop.wait_json(owner, lambda item: item.get('event') == 'restored', 5)
                report['clipboard_restored'] = all(
                    desktop.read_bounded(['wl-paste', '--no-newline', '--type', mime]) == value
                    for mime, value in saved.items())
            else:
                desktop.command(['wl-copy', '--clear'])
                report['clipboard_restored'] = True
        finally:
            report['passed'] = report['passed'] and report.get('clipboard_restored', False)
            with args.output.open('x') as output:
                json.dump(report, output, ensure_ascii=False, indent=2)
                output.write('\n')
    print(json.dumps(report, ensure_ascii=False))
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
