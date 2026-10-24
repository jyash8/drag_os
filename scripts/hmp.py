#!/usr/bin/env python3
"""Run HMP commands over QMP.

`screendump` cannot capture a virgl scanout (it is a host GL texture, not a
QEMU console surface -- documented in crates/tools/src/qemu.rs). But HMP's `xp`
reads *guest physical memory*, which is the buffer the compositor actually
rendered into. That gives a non-perturbing oracle: if the BO is clean and the
screen is sheared, the fault is in scanout/sampling; if the BO is itself
sheared, it is upstream.

Lives in the repo because /tmp has been wiped repeatedly mid-session.
"""
import json, socket, sys

SOCK = "/tmp/dragos-qmp.sock"


class Qmp:
    def __init__(self, path=SOCK):
        self.s = socket.socket(socket.AF_UNIX)
        self.s.connect(path)
        self.f = self.s.makefile("rwb")
        self.send("qmp_capabilities")

    def send(self, cmd, **args):
        m = {"execute": cmd}
        if args:
            m["arguments"] = args
        self.f.write((json.dumps(m) + "\n").encode())
        self.f.flush()
        while True:
            line = self.f.readline()
            if not line:
                return None
            r = json.loads(line)
            # Skip the QMP greeting and async events: only a real command
            # reply has a "return"/"error" key. Without this, every reply
            # lags one command behind (the greeting reads as the first
            # response), which is why `xp`/`info status` came back {}.
            if "return" in r or "error" in r:
                return r

    def hmp(self, cmd):
        return self.send("human-monitor-command", **{"command-line": cmd})


if __name__ == "__main__":
    q = Qmp()
    for c in sys.argv[1:]:
        print(json.dumps(q.hmp(c)))
        print("---")
