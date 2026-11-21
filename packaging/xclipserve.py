#!/usr/bin/env python3
"""Own the X11 CLIPBOARD and serve TEXT to whoever asks, like `xclip -i`, for a few seconds.
Lets the smoke test paste through the real system clipboard without xclip installed.
Usage: xclipserve.py TEXT [seconds]"""
import sys
import time

from Xlib import X, Xatom, display
from Xlib.protocol import event

text = sys.argv[1].encode()
until = time.time() + float(sys.argv[2] if len(sys.argv) > 2 else 15)
d = display.Display()
w = d.screen().root.create_window(0, 0, 1, 1, 0, X.CopyFromParent)
CLIP, UTF8, TARGETS = (d.intern_atom(n) for n in ("CLIPBOARD", "UTF8_STRING", "TARGETS"))
TEXTS = {UTF8, Xatom.STRING, d.intern_atom("TEXT"), d.intern_atom("text/plain;charset=utf-8"), d.intern_atom("text/plain")}
w.set_selection_owner(CLIP, X.CurrentTime)
d.sync()
if d.get_selection_owner(CLIP) != w:
    raise SystemExit("could not own the clipboard")
print("ready", flush=True)
served = 0
while time.time() < until:
    if not d.pending_events():
        time.sleep(0.02)
        continue
    e = d.next_event()
    if e.type == X.SelectionClear:
        break
    if e.type != X.SelectionRequest:
        continue
    prop = e.property or e.target
    if e.target == TARGETS:
        e.requestor.change_property(prop, Xatom.ATOM, 32, [TARGETS, *TEXTS])
    elif e.target in TEXTS:
        e.requestor.change_property(prop, e.target, 8, text)
        served += 1
        print("served", e.target, flush=True)
    else:
        prop = X.NONE
    ev = event.SelectionNotify(time=e.time, requestor=e.requestor, selection=e.selection, target=e.target, property=prop)
    e.requestor.send_event(ev)
    d.flush()
print(f"served {served}", flush=True)
