"""Fixed-size in-place writer used only as the native proof's held child."""
import os
import signal
import sys
import time

running = True


def stop(_signal, _frame):
    global running
    running = False


signal.signal(signal.SIGTERM, stop)
path = sys.argv[1]
with open(path, "r+b", buffering=0) as handle:
    size = os.fstat(handle.fileno()).st_size
    if size != 4096:
        raise SystemExit("unexpected writer file size")
    print("ready", flush=True)
    sequence = 0
    while running:
        sequence += 1
        record = f"{sequence:020d}".encode().ljust(size, b".")
        handle.seek(0)
        handle.write(record)
        os.fsync(handle.fileno())
        time.sleep(0.02)
    print(sequence, flush=True)
