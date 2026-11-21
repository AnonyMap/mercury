#!/usr/bin/env python3
"""Minimal X11 input driver (XTEST) for GUI smoke tests: click and type."""
import sys
import time

from Xlib import X, XK, display
from Xlib.ext import xtest

d = display.Display()


def click(x, y):
    xtest.fake_input(d, X.MotionNotify, x=x, y=y)
    d.sync()
    time.sleep(0.05)
    xtest.fake_input(d, X.ButtonPress, 1)
    xtest.fake_input(d, X.ButtonRelease, 1)
    d.sync()


def focus(title):
    """No window manager under Xvfb, so hand keyboard focus to the window ourselves."""
    def walk(w):
        try:
            if (w.get_wm_name() or "") == title:
                return w
        except Exception:
            pass
        for c in w.query_tree().children:
            r = walk(c)
            if r:
                return r
        return None
    w = walk(d.screen().root)
    if w is None:
        raise SystemExit(f"no window titled {title!r}")
    w.set_input_focus(X.RevertToParent, X.CurrentTime)
    d.sync()


def key(name, shift=False, mod=None):
    code = d.keysym_to_keycode(XK.string_to_keysym(name))
    mods = [m for m in (mod, "Shift_L" if shift else None) if m]
    mcodes = [d.keysym_to_keycode(XK.string_to_keysym(m)) for m in mods]
    for mc in mcodes:
        xtest.fake_input(d, X.KeyPress, mc)
    xtest.fake_input(d, X.KeyPress, code)
    xtest.fake_input(d, X.KeyRelease, code)
    for mc in reversed(mcodes):
        xtest.fake_input(d, X.KeyRelease, mc)
    d.sync()
    time.sleep(0.02)


SPECIAL = {" ": "space", "-": "minus", ".": "period", "/": "slash", ",": "comma", "?": "question", "'": "apostrophe"}


def type_text(s):
    for ch in s:
        if ch in SPECIAL:
            key(SPECIAL[ch], shift=(ch == "?"))
        elif ch.isupper():
            key(ch.lower(), shift=True)
        else:
            key(ch)


if __name__ == "__main__":
    args = sys.argv[1:]
    i = 0
    while i < len(args):
        a = args[i]
        if a == "click":
            click(int(args[i + 1]), int(args[i + 2]))
            i += 3
        elif a == "type":
            type_text(args[i + 1])
            i += 2
        elif a == "key":
            key(args[i + 1])
            i += 2
        elif a == "ctrl":
            key(args[i + 1], mod="Control_L")
            i += 2
        elif a == "focus":
            focus(args[i + 1])
            i += 2
        elif a == "sleep":
            time.sleep(float(args[i + 1]))
            i += 2
        else:
            raise SystemExit(f"unknown action {a}")
