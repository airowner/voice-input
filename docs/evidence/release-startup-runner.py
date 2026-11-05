"""Explicit owned release startup/CLI probe. No recording, model load or injection."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

root = Path(__file__).resolve().parents[2]
binary = Path(sys.argv[1]).resolve()
output = Path(sys.argv[2]).resolve()
assert binary.name == 'voice-input' and binary.is_relative_to(root)
assert output.parent == root / 'docs/evidence' and not output.exists()
ctl = binary.with_name('voice-inputctl')
checks = {}
with tempfile.TemporaryDirectory(prefix='voice-release-ui-') as temporary:
    env = {**os.environ, 'XDG_DATA_HOME': temporary+'/data',
           'XDG_CONFIG_HOME': temporary+'/config', 'XDG_CACHE_HOME': temporary+'/cache'}
    process = subprocess.Popen([str(binary)], env=env, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True)
    def call(action):
        result = subprocess.run([str(ctl), action], env=env, capture_output=True,
                                text=True, timeout=2)
        return result.returncode, json.loads(result.stdout) if result.stdout.strip() else None
    try:
        deadline = time.monotonic()+15
        while True:
            status, reply = call('status')
            if status == 0 and reply['code'] == 'accepted':
                break
            assert time.monotonic() < deadline and process.poll() is None, 'startup failed'
            time.sleep(.05)
        checks['real_cli_status'] = reply['status'] == {'state': 'ready', 'busy': False}
        for index, action in enumerate(['stop', 'stop', 'cancel']):
            status, reply = call(action)
            checks[f'idle_{action}_{index}'] = status == 0 and reply['code'] == 'accepted'
        # Missing models must reject before opening the microphone.
        status, reply = call('start')
        checks['unconfigured_start_rejected'] = status != 0 and reply['code'] == 'invalid'
        status, reply = call('status')
        checks['still_ready'] = status == 0 and reply['status']['state'] == 'ready'
        second = subprocess.run([str(binary)], env=env, capture_output=True, text=True, timeout=5)
        checks['single_instance_rejected'] = second.returncode == 1 and process.poll() is None
        # Allow the real WebKit window to stay alive briefly. DOM is covered by
        # the separate debug self-report; this does not assert visual inspection.
        time.sleep(1)
        checks['window_process_alive'] = process.poll() is None
    finally:
        process.terminate()
        try:
            stdout, stderr = process.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            stdout, stderr = process.communicate(timeout=5)
    checks['release_process_reaped'] = process.poll() is not None
    # The same session is now available, without opening another GUI instance.
    status, reply = call('status')
    checks['cli_reports_not_running_after_exit'] = status != 0
record = {'scope': 'actual Linux release startup and private CLI developer verification; isolated user data, no audio/model/injection; not desktop V01/V07/V09 acceptance',
          'binary': str(binary.relative_to(root)), 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
          'checks': checks, 'passed': all(checks.values()), 'stdout': stdout, 'stderr': stderr,
          'shutdown_method': 'SIGTERM to owned idle application only; no active capture or worker'}
with output.open('x') as target:
    json.dump(record, target, ensure_ascii=False, indent=2)
    target.write('\n')
print(json.dumps({'passed': record['passed'], 'checks': checks}))
raise SystemExit(0 if record['passed'] else 1)
