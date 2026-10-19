#!/usr/bin/env python3
"""Drive the workbench that scripts/workbench-sandbox.sh started.

Controls are found by their accessible names (AccessKit over AT-SPI), and
clicks and keys go through XTEST, the same path a user's pointer takes.
Screenshots capture the whole private display.

  shot NAME                       save SHOTS/NAME.png and print its path
  tree [TEXT]                     list nodes: role, name, screen rect, actions
  find NAME                       list nodes with exactly this name
  wait NAME [--timeout S]         wait until a node with this name exists
  press NAME [--role R] [--index N]
                                  click the centre of a named control
  click X Y | dclick X Y | move X Y
  scroll X Y up|down N            wheel steps over a point
  key KEY...                      keysyms or chords: Escape, Tab, ctrl+a
  type TEXT                       type into the focused field
  close                           close the window as a window manager would
  resize W H                      resize the window, for example 900 600

NAME matching is exact; --contains matches a substring. With an external
daemon, pressing Launch, Retry or Cancel needs --force.
"""
import argparse
import os
import shlex
import subprocess
import sys
import time
import warnings

warnings.filterwarnings('ignore', category=DeprecationWarning)
MUTATIONS = ('Launch', 'Retry', 'Cancel')


def sandbox(path):
    env = {}
    try:
        with open(os.path.join(path, 'env')) as file:
            for line in file:
                key, _, value = line.rstrip('\n').partition('=')
                env[key] = shlex.split(value)[0] if value else ''
    except FileNotFoundError:
        sys.exit(f'no sandbox in {path}; run scripts/workbench-sandbox.sh start')
    return env


def xdisplay(env):
    try:
        from Xlib import display
    except ImportError:
        sys.exit('missing python3-xlib')
    return display.Display(env['DISPLAY'])


def window(d):
    for w in d.screen().root.query_tree().children:
        if w.get_wm_name() == 'Playtime':
            return w
    sys.exit('no Playtime window on the sandbox display')


def pointer(env, x, y, button=None, clicks=1):
    from Xlib import X
    from Xlib.ext import xtest
    d = xdisplay(env)
    xtest.fake_input(d, X.MotionNotify, x=int(x), y=int(y))
    d.sync()
    time.sleep(0.08)
    for _ in range(clicks if button else 0):
        xtest.fake_input(d, X.ButtonPress, button)
        d.sync()
        time.sleep(0.05)
        xtest.fake_input(d, X.ButtonRelease, button)
        d.sync()
        time.sleep(0.05)
    d.close()


def focus(env):
    # Without a window manager nothing gives the window keyboard focus.
    from Xlib import X
    d = xdisplay(env)
    window(d).set_input_focus(X.RevertToParent, X.CurrentTime)
    d.sync()
    d.close()


def keys(env, names):
    from Xlib import X, XK
    from Xlib.ext import xtest
    focus(env)
    d = xdisplay(env)
    aliases = {'ctrl': 'Control_L', 'shift': 'Shift_L', 'alt': 'Alt_L'}
    for name in names:
        codes = []
        for part in name.split('+'):
            symbol = XK.string_to_keysym(aliases.get(part, part))
            if not symbol:
                sys.exit(f'unknown key {part}')
            codes.append(d.keysym_to_keycode(symbol))
        for code in codes:
            xtest.fake_input(d, X.KeyPress, code)
            d.sync()
            time.sleep(0.03)
        for code in reversed(codes):
            xtest.fake_input(d, X.KeyRelease, code)
            d.sync()
            time.sleep(0.03)
    d.close()


def type_text(env, text):
    from Xlib import X, XK
    from Xlib.ext import xtest
    focus(env)
    d = xdisplay(env)
    shift = d.keysym_to_keycode(XK.string_to_keysym('Shift_L'))
    for ch in text:
        # Printable ASCII keysyms equal their code points.
        symbol = {'\n': 'Return', '\t': 'Tab'}.get(ch)
        symbol = XK.string_to_keysym(symbol) if symbol else ord(ch)
        code = d.keysym_to_keycode(symbol)
        if not code:
            sys.exit(f'cannot type {ch!r} on this keymap')
        # A shifted symbol sits in the second column of its keycode's mapping.
        shifted = d.keycode_to_keysym(code, 0) != symbol
        if shifted:
            xtest.fake_input(d, X.KeyPress, shift)
        xtest.fake_input(d, X.KeyPress, code)
        xtest.fake_input(d, X.KeyRelease, code)
        if shifted:
            xtest.fake_input(d, X.KeyRelease, shift)
        d.sync()
        time.sleep(0.02)
    d.close()


def atspi(env):
    os.environ['DBUS_SESSION_BUS_ADDRESS'] = env['DBUS_SESSION_BUS_ADDRESS']
    try:
        import gi
        gi.require_version('Atspi', '2.0')
        from gi.repository import Atspi, GLib
    except (ImportError, ValueError):
        sys.exit('missing gir1.2-atspi-2.0 (python3-gi)')
    # AccessKit has no AT-SPI cache object, and libatspi warns whenever it asks.
    GLib.log_set_handler('dbind', GLib.LogLevelFlags.LEVEL_WARNING, lambda *_: None)
    Atspi.init()
    return Atspi


def nodes(env, handles=False):
    """Yields (depth, role, name, rect, actions[, node]) for the workbench's nodes."""
    Atspi = atspi(env)
    desktop = Atspi.get_desktop(0)

    def walk(node, depth):
        try:
            role, name = node.get_role_name(), node.get_name() or ''
        except Exception:
            return
        rect = None
        try:
            r = node.get_extents(Atspi.CoordType.SCREEN)
            rect = (r.x, r.y, r.width, r.height)
        except Exception:
            pass
        actions = []
        try:
            actions = [node.get_action_name(i) for i in range(node.get_n_actions())]
        except Exception:
            pass
        yield (depth, role, name, rect, actions, node) if handles else (depth, role, name, rect, actions)
        try:
            count = node.get_child_count()
        except Exception:
            count = 0
        for i in range(count):
            child = node.get_child_at_index(i)
            if child is not None:
                yield from walk(child, depth + 1)

    for i in range(desktop.get_child_count()):
        app = desktop.get_child_at_index(i)
        if app is not None:
            yield from walk(app, 0)


def matching(env, name, contains=False, role=None):
    return [(r, n, rect, node) for _, r, n, rect, _, node in nodes(env, handles=True)
            if (name in n if contains else n == name) and (role is None or r == role)]


def visible(env, rect):
    from Xlib import display
    d = display.Display(env['DISPLAY'])
    g = window(d).get_geometry()
    d.close()
    x, y, w, h = rect
    return w > 0 and h > 0 and x >= g.x and y >= g.y and x + w <= g.x + g.width and y + h <= g.y + g.height


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--dir', default=os.environ.get('PLAYTIME_SANDBOX', f'/tmp/playtime-workbench-{os.getuid()}'))
    parser.add_argument('command')
    parser.add_argument('args', nargs='*')
    parser.add_argument('--role')
    parser.add_argument('--index', type=int, default=0)
    parser.add_argument('--contains', action='store_true')
    parser.add_argument('--timeout', type=float, default=30.0)
    parser.add_argument('--force', action='store_true')
    a = parser.parse_args()
    env = sandbox(a.dir)
    command, args = a.command, a.args

    def need(count):
        if len(args) != count:
            parser.error(f'{command} takes {count} argument(s)')

    if command == 'shot':
        need(1)
        path = os.path.join(env['SHOTS'], args[0] + '.png')
        subprocess.run(['import', '-display', env['DISPLAY'], '-window', 'root', path], check=True)
        print(path)
    elif command == 'tree':
        text = args[0].lower() if args else None
        for depth, role, name, rect, actions in nodes(env):
            line = f"{'  ' * depth}{role} {name!r} {rect} {actions or ''}"
            if text is None or text in line.lower():
                print(line)
    elif command == 'find':
        need(1)
        for role, name, rect, _ in matching(env, args[0], a.contains, a.role):
            print(role, repr(name), rect)
    elif command == 'wait':
        need(1)
        deadline = time.monotonic() + a.timeout
        while not matching(env, args[0], a.contains, a.role):
            if time.monotonic() > deadline:
                sys.exit(f'no {args[0]!r} after {a.timeout:.0f} s')
            time.sleep(0.25)
        print('found', repr(args[0]))
    elif command == 'press':
        need(1)
        found = matching(env, args[0], a.contains, a.role)
        if a.index >= len(found):
            near = sorted({n for _, _, n, _, _ in nodes(env) if args[0].lower() in n.lower()})
            sys.exit(f'{len(found)} nodes named {args[0]!r}; similar: {near[:12]}')
        role, name, rect, node = found[a.index]
        if env.get('EXTERNAL') == '1' and name.startswith(MUTATIONS) and not a.force:
            sys.exit(f'refusing to press {name!r} on an external daemon without --force')
        if not visible(env, rect):
            # AT-SPI ScrollTo becomes AccessKit's ScrollIntoView, which egui honours.
            node.scroll_to(atspi(env).ScrollType.ANYWHERE)
            time.sleep(0.5)
            found = matching(env, args[0], a.contains, a.role)
            if a.index >= len(found) or not visible(env, found[a.index][2]):
                sys.exit(f'{name!r} is outside the window at {rect}; scroll it into view')
            rect = found[a.index][2]
        x, y, w, h = rect
        print(f'pressing {role} {name!r} at ({x + w // 2}, {y + h // 2})')
        pointer(env, x + w // 2, y + h // 2, button=1)
    elif command in ('click', 'dclick', 'move'):
        need(2)
        pointer(env, args[0], args[1], button=None if command == 'move' else 1,
                clicks=2 if command == 'dclick' else 1)
    elif command == 'scroll':
        need(4)
        pointer(env, args[0], args[1], button=4 if args[2] == 'up' else 5, clicks=int(args[3]))
        time.sleep(0.5)
    elif command == 'key':
        keys(env, args)
    elif command == 'type':
        need(1)
        type_text(env, args[0])
    elif command == 'close':
        from Xlib import X, protocol
        d = xdisplay(env)
        w = window(d)
        event = protocol.event.ClientMessage(
            window=w, client_type=d.intern_atom('WM_PROTOCOLS'),
            data=(32, [d.intern_atom('WM_DELETE_WINDOW'), X.CurrentTime, 0, 0, 0]))
        w.send_event(event, event_mask=X.NoEventMask)
        d.sync()
        d.close()
    elif command == 'resize':
        need(2)
        d = xdisplay(env)
        window(d).configure(width=int(args[0]), height=int(args[1]))
        d.sync()
        d.close()
    else:
        parser.error(f'unknown command {command}')


if __name__ == '__main__':
    main()
