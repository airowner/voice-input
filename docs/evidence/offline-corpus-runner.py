import errno, fcntl, hashlib, json, os, pathlib, socket, struct, subprocess, time
root = pathlib.Path(__file__).resolve().parents[2]
os.chdir(root)
report_path = root / 'docs/evidence/v10-offline-network.json'
if report_path.exists():
    raise SystemExit('Evidence already exists')
control = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
interfaces = {name: struct.unpack('h', fcntl.ioctl(control, 0x8913, struct.pack('16sh', name.encode(), 0))[16:18])[0] for _, name in socket.if_nameindex()}
control.close()
assert set(interfaces) == {'lo'} and not interfaces['lo'] & 1, interfaces
probe = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
probe.settimeout(.5)
try:
    probe.connect(('198.51.100.1', 443))
    raise RuntimeError('External route unexpectedly available')
except OSError as error:
    blocked_errno = error.errno
    assert blocked_errno == errno.ENETUNREACH, error
finally:
    probe.close()
net_files = ['route', 'ipv6_route', 'tcp', 'tcp6', 'udp', 'udp6']
def state():
    return {name: pathlib.Path('/proc/net', name).read_text() for name in net_files}
record = {'scope': 'Linux developer corpus under a kernel isolated network namespace; no microphone or paste; partial V03, not desktop acceptance', 'network_namespace': os.readlink('/proc/self/ns/net'), 'interfaces': interfaces, 'external_probe_errno': blocked_errno, 'external_probe_destination': 'RFC5737 TEST-NET-2, no route', 'before': state(), 'audit_method': 'Inherited network namespace confinement with loopback down and no external interfaces/routes; no syscall tracer or online background-connection audit', 'corpus_binary_sha256': hashlib.sha256((root/'target/debug/examples/corpus').read_bytes()).hexdigest(), 'background_release_build': True}
start = time.monotonic()
run = subprocess.run([str(root/'target/debug/examples/corpus'), '--output', 'docs/evidence/v10-corpus-local-offline.json'], timeout=1800, check=False)
record.update(returncode=run.returncode, elapsed_seconds=round(time.monotonic()-start, 3), after=state())
with report_path.open('x') as f:
    json.dump(record, f, ensure_ascii=False, indent=2)
    f.write('\n')
raise SystemExit(run.returncode)
