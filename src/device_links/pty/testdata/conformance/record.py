#!/usr/bin/env python3
"""Regenerates the terminal conformance corpus (`<case>.<cols>.bin`).

Real programs run on a pty of the given size and their raw output is captured;
synthetic cases are byte generators that reproduce the output *patterns* of
programs we cannot run here (Ink, Ratatui, tmux, fzf, lazygit, btop).
Run: record.py [case ...]   (all cases when none are named)

The grids the emulators are compared against are produced by the Rust test
`conformance` (DESKLINK_BLESS=1) from the `vt100` crate; nothing here depends
on a terminal emulator.
"""
import fcntl, os, pty, select, shutil, signal, struct, subprocess, sys, tempfile, termios, time

HERE = os.path.dirname(os.path.abspath(__file__))
WIDTHS = (40, 60, 80, 120)
ROWS = 30
ESC = "\x1b"
CSI = ESC + "["


# ---- real programs on a pty ---------------------------------------------------------------------

marks = []   # (byte offset, cols, rows) of the last run


def run_pty(argv, cols, rows=ROWS, keys=(), resizes=(), env=None, wall=6.0, cwd=None, netns=False):
    """keys: [(delay_s, bytes)] ; resizes: [(delay_s, cols, rows)]. Returns the raw output."""
    e = {"TERM": "xterm-256color", "COLORTERM": "truecolor", "LANG": "C.UTF-8", "PATH": os.environ["PATH"],
         "HOME": os.environ.get("HOME", "/tmp")}
    e.update(env or {})
    if netns:
        argv = ["unshare", "-rn"] + argv
    pid, fd = pty.fork()
    if pid == 0:
        if cwd:
            os.chdir(cwd)
        os.execvpe(argv[0], argv, e)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    out = bytearray()
    marks.clear()
    events = sorted([(d, "k", k) for d, k in keys] + [(d, "r", (c, r)) for d, c, r in resizes], key=lambda x: x[0])
    start = time.time()
    done = False
    while time.time() - start < wall:
        now = time.time() - start
        while events and events[0][0] <= now:
            _, kind, v = events.pop(0)
            if kind == "k":
                os.write(fd, v)
            else:
                fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", v[1], v[0], 0, 0))
                marks.append((len(out), v[0], v[1]))
                os.kill(pid, signal.SIGWINCH)
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                data = os.read(fd, 65536)
            except OSError:
                break
            if not data:
                break
            out += data
            # A real terminal answers these; programs wait for them at startup.
            if b"\x1b[6n" in data:
                os.write(fd, b"\x1b[1;1R")
            if b"\x1b[c" in data or b"\x1b[0c" in data:
                os.write(fd, b"\x1b[?62;c")
            if b"\x1b[>0q" in data or b"\x1b[>q" in data:
                os.write(fd, b"\x1bP>|xterm(388)\x1b\\")
        else:
            try:
                if os.waitpid(pid, os.WNOHANG)[0]:
                    done = True
                    break
            except ChildProcessError:
                break
    try:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
    except Exception:
        pass
    return bytes(out)


def workdir():
    d = tempfile.mkdtemp(prefix="dlconf-")
    for name in ["src", "docs", "target", "README.md", "Cargo.toml", "main.rs", "a-very-long-file-name-that-wraps.txt", "日本語.txt", "notes.md"]:
        p = os.path.join(d, name)
        if name in ("src", "docs", "target"):
            os.mkdir(p)
        else:
            open(p, "w").write("x\n")
    os.chmod(os.path.join(d, "main.rs"), 0o755)
    os.symlink("README.md", os.path.join(d, "link"))
    return d


LONG_TEXT = "".join(
    "line %02d: \x1b[1;3%dm%s\x1b[0m the quick brown fox jumps over the lazy dog 0123456789 ░▒▓█ │┌─┐\n" % (i, 1 + i % 6, "bold" * (1 + i % 3))
    for i in range(80))

PY_SOURCE = '''import os

def greet(name: str) -> str:
    """Say hello."""
    # a long comment line that is certainly longer than forty columns wide and wraps in vim
    return f"hello, {name}!"  # trailing

class Thing:
    def __init__(self, value=42):
        self.value = value


if __name__ == "__main__":
    print(greet(os.environ.get("USER", "world")))
''' * 3

CURSES_APP = r'''
import curses, locale
locale.setlocale(locale.LC_ALL, "")
def main(s):
    curses.start_color(); curses.use_default_colors()
    for i in range(1, 8): curses.init_pair(i, i, -1)
    curses.curs_set(0)
    h, w = s.getmaxyx()
    s.box()
    s.addstr(0, 2, " curses demo ", curses.A_BOLD)
    for i in range(1, min(h - 1, 12)):
        s.addstr(i, 2, ("row %d " % i) + "░▒▓█" * 3 + " 日本語", curses.color_pair(1 + i % 7))
    s.addstr(h - 2, 2, "reverse", curses.A_REVERSE)
    s.addstr(h - 2, 12, "underline", curses.A_UNDERLINE)
    s.refresh()
    s.getch()
curses.wrapper(main)
'''


# ---- synthetic generators (cols, rows) -> bytes -------------------------------------------------

def g_ink(cols, rows):
    """Ink-style: a status block rewritten in place with CUU + EL, spinner, rounded box, text wrapped by the app."""
    o = []
    def box(lines):
        inner = cols - 2
        out = ["\x1b[38;2;215;119;87m╭" + "─" * inner + "╮\x1b[0m"]
        for l in lines:
            l = l[:inner].ljust(inner)
            out.append("\x1b[38;2;215;119;87m│\x1b[0m" + l + "\x1b[38;2;215;119;87m│\x1b[0m")
        out.append("\x1b[38;2;215;119;87m╰" + "─" * inner + "╯\x1b[0m")
        return out
    prev = 0
    frames = "✻✽✳·"
    msg = "Thinking about the problem and reading some files from the repository to find where it is"
    for i in range(12):
        body = ["\x1b[1m> hello there\x1b[0m"] if i < 3 else []
        text = [frames[i % 4] + " Percolating… (" + str(i) + "s · esc to interrupt)"]
        wrapped = []
        w = cols
        for chunk in range(0, len(msg), w):
            wrapped.append(msg[chunk:chunk + w])
        lines = body + text + wrapped + box(["> " + "type here " * 3, "? for shortcuts"])
        if prev:
            o.append(CSI + "2K" + (CSI + "1A" + CSI + "2K") * (prev - 1) + "\r")
        o.append("\r\n".join(lines))
        prev = len(lines)
    o.append("\r\n")
    return "".join(o).encode()


def g_ink_naive(cols, rows):
    """An app that writes lines wider than the terminal and redraws with relative moves: wrap must be exact."""
    o = []
    prev = 0
    for i in range(8):
        lines = ["frame %d: " % i + "W" * (cols + 7), "x" * cols, "y" * (cols - 1) + "│", "z" * (cols * 2 + 3), "done"]
        # visual rows the terminal used: each line ceil(len/cols), the last line has no newline
        used = sum(max(1, -(-len(l) // cols)) for l in lines)
        if prev:
            o.append(CSI + "%dA" % (prev - 1) + "\r" + CSI + "J")
        o.append("\r\n".join(lines))
        prev = used
    return "".join(o).encode()


def g_ratatui(cols, rows):
    o = [CSI + "?1049h", CSI + "?25l", CSI + "?1000h" + CSI + "?1006h", CSI + "2J", CSI + "H"]
    def at(r, c, s):
        return CSI + "%d;%dH" % (r + 1, c + 1) + s
    def frame(n):
        out = [CSI + "?2026h"]
        out.append(at(0, 0, "\x1b[38;2;100;180;255m┌" + "─" * (cols - 2) + "┐\x1b[0m"))
        for r in range(1, rows - 1):
            out.append(at(r, 0, "\x1b[38;2;100;180;255m│\x1b[0m"))
            out.append(at(r, cols - 1, "\x1b[38;2;100;180;255m│\x1b[0m"))
        out.append(at(rows - 1, 0, "\x1b[38;2;100;180;255m└" + "─" * (cols - 2) + "┘\x1b[0m"))
        out.append(at(0, 2, "\x1b[1m Codex \x1b[0m"))
        inner = cols - 4
        for i in range(min(rows - 6, 8)):
            out.append(at(2 + i, 2, ("\x1b[48;2;40;44;52m" if i == n % 8 else "") + ("item %d %s" % (i, "·" * 40))[:inner].ljust(inner) + "\x1b[0m"))
        out.append(at(rows - 3, 2, "╭" + "─" * (inner - 2) + "╮"))
        out.append(at(rows - 2, 2, "│ > " + "ask me anything"[:inner - 6].ljust(inner - 4) + "│"))
        out.append(CSI + "?2026l")
        return "".join(out)
    for n in range(4):
        o.append(frame(n))
    return "".join(o).encode()


def g_alt_leave(cols, rows):
    return (b"before\r\n" + (CSI + "?1049h" + CSI + "2J" + CSI + "H" + "alt screen\r\n" + CSI + "5;5Hmoved" + CSI + "?1049l" + "after\r\n")).encode() if False else (
        "before\r\n" + CSI + "?1049h" + CSI + "2J" + CSI + "H" + "alt screen\r\n" + CSI + "5;5Hmoved" + CSI + "?1049l" + "after\r\n").encode()


def g_tmux(cols, rows):
    o = [CSI + "?1049h", CSI + "2J", CSI + "H", CSI + "1;%dr" % (rows - 1)]
    for i in range(1, 12):
        o.append("$ echo line %d %s\r\n" % (i, "abc" * 20))
    status = " [0] 0:bash* 1:vim- ".ljust(cols - 12) + " 12:30 05-Nov "
    o.append(CSI + "%d;1H" % rows + "\x1b[30;42m" + status[:cols].ljust(cols) + "\x1b[0m")
    o.append(CSI + "%d;1H" % (rows - 1))
    for i in range(rows):                       # scroll inside the region
        o.append("\r\nscrolled %d" % i)
    o.append(CSI + "r")
    return "".join(o).encode()


def g_fzf(cols, rows):
    o = [CSI + "?25l"]
    items = ["src/main.rs", "src/lib.rs", "docs/README.md", "Cargo.toml", "日本語/ファイル.txt", "a/very/long/path/" + "x" * 60]
    o.append("> \r\n")
    for i, it in enumerate(items):
        o.append(("\x1b[7m> \x1b[0m" if i == 0 else "  ") + it[:cols - 2] + "\r\n")
    o.append("\x1b[2m  %d/%d\x1b[0m" % (len(items), len(items)))
    o.append(CSI + "%dA" % (len(items) + 1) + "\r" + CSI + "2C" + CSI + "?25h" + "ma")
    o.append(CSI + "%dB" % (len(items) + 1) + "\r" + CSI + "2K")
    return "".join(o).encode()


def g_lazygit(cols, rows):
    o = [CSI + "?1049h", CSI + "2J"]
    third = cols // 3
    def at(r, c, s): return CSI + "%d;%dH" % (r + 1, c + 1) + s
    for panel, (c0, w, r0, h, title) in enumerate([(0, third, 0, rows // 2, "Status"), (0, third, rows // 2, rows - rows // 2 - 1, "Files"), (third, cols - third, 0, rows - 1, "Log")]):
        o.append(at(r0, c0, "╭─" + title + "─" * (w - len(title) - 3) + "╮"))
        for r in range(1, h - 1):
            o.append(at(r0 + r, c0, "│" + (" commit %02d  fix thing" % r)[:w - 2].ljust(w - 2) + "│"))
        o.append(at(r0 + h - 1, c0, "╰" + "─" * (w - 2) + "╯"))
    o.append(at(rows - 1, 0, "\x1b[34mOpen menu: x  Keybindings: ?\x1b[0m"))
    return "".join(o).encode()


def g_btop(cols, rows):
    o = [CSI + "?1049h", CSI + "2J", CSI + "?25l"]
    def at(r, c, s): return CSI + "%d;%dH" % (r + 1, c + 1) + s
    o.append(at(0, 0, "┌─cpu─" + "─" * (cols - 7) + "┐"))
    braille = "⣀⣄⣤⣦⣶⣷⣿⡇⠇⠃"
    for r in range(1, 8):
        row = "".join(braille[(r * 3 + c) % len(braille)] for c in range(cols - 2))
        o.append(at(r, 0, "│\x1b[38;2;80;200;120m" + row + "\x1b[0m│"))
    o.append(at(8, 0, "└" + "─" * (cols - 2) + "┘"))
    for r in range(9, rows - 1):
        o.append(at(r, 0, "\x1b[1m%6d\x1b[0m user  %s  %5.1f%%" % (1000 + r, "proc" + str(r), r * 1.5)))
    return "".join(o).encode()


def g_wide(cols, rows):
    lines = ["ascii abc", "日本語のテキスト", "한국어 텍스트", "emoji 😀 👍 🎉 end", "zwj 👨‍👩‍👧 end", "combining é e\u0301 a\u0308 end",
             "ambig ±×÷ ○● end", "box ┌─┬─┐ ├─┼─┤ └─┴─┘ ╔═╗ ║ ╚═╝ ╭─╮ ╰─╯", "block ▀▄█▌▐ ░▒▓ ▁▂▃▄▅▆▇█",
             "braille ⠀⠁⠂⠃⡇⣿", "spin ✻ ✽ ✳ ⏺ ⎿ ❯ ● ○ ✓ ✗", "x" * (cols - 1) + "日本", "y" * (cols - 2) + "日本", "z" * cols + "w"]
    return ("\r\n".join(lines) + "\r\n").encode()


def g_sgr(cols, rows):
    o = []
    for n in (1, 2, 3, 4, 5, 7, 9, 21):
        o.append("\x1b[%dmattr%d\x1b[0m " % (n, n))
    o.append("\r\n")
    for i in (30, 31, 32, 33, 34, 35, 36, 37, 90, 97):
        o.append("\x1b[%dm%d\x1b[0m " % (i, i))
    o.append("\r\n\x1b[38;5;208mc256\x1b[48;5;21m bg\x1b[0m \x1b[38;2;12;34;56mtrue\x1b[0m \x1b[38:2::200:100:50mcolon\x1b[0m \x1b[48:5:99mc\x1b[0m\r\n")
    o.append("\x1b[1;31;47mbold red on white\x1b[22;39;49m plain\x1b[0m\r\n\x1b[7mrev\x1b[27m not\x1b[0m\r\n")
    return "".join(o).encode()


def g_edit(cols, rows):
    o = [CSI + "2J", CSI + "H"]
    for i in range(1, 11):
        o.append("line%d-" % i + "x" * 10 + "\r\n")
    o.append(CSI + "3;1H" + CSI + "2L")                # IL
    o.append(CSI + "6;1H" + CSI + "1M")                # DL
    o.append(CSI + "1;3H" + CSI + "3@" + "INS")        # ICH
    o.append(CSI + "2;3H" + CSI + "2P")                # DCH
    o.append(CSI + "4;5H" + CSI + "3X")                # ECH
    o.append(CSI + "5;1H" + "ab" + CSI + "5b")         # REP
    o.append(CSI + "8;1H" + "\x1b7" + CSI + "10;10H" + "saved" + "\x1b8" + "restored")
    o.append(CSI + "9;1H" + CSI + "s" + CSI + "11;3H" + "x" + CSI + "u" + "back")
    o.append(CSI + "3;8r" + CSI + "?6h" + CSI + "1;1H" + "origin")  # DECSTBM + DECOM
    o.append(CSI + "r" + CSI + "?6l")
    o.append(CSI + "20;5H" + "CHA" + CSI + "10G" + "x" + CSI + "2E" + "CNL" + CSI + "1F" + "CPL" + CSI + "25d" + "VPA" + CSI + "30`" + "HPA")
    o.append(CSI + "13;1H" + "\x1b(0" + "lqqqk\r\nx   x\r\nmqqqj" + "\x1b(B" + " done")
    o.append(CSI + "2;1H" + CSI + "0J" + CSI + "1K")
    return "".join(o).encode()


def g_sync(cols, rows):
    o = []
    for n in range(3):
        o.append(CSI + "?2026h" + CSI + "2J" + CSI + "H" + ("frame %d " % n) * 3 + "\r\n" + "─" * (cols // 2) + CSI + "?2026l")
    return "".join(o).encode()


def g_prompt(cols, rows):
    """A shell session: coloured prompt, wrapped command, ls output, ^C, bracketed paste on."""
    o = [CSI + "?2004h", "\x1b[1;32muser@host\x1b[0m:\x1b[1;34m~/project\x1b[0m$ ", "echo " + "word " * 30, "\r\n", "word " * 30, "\r\n",
         "\x1b[1;32muser@host\x1b[0m:\x1b[1;34m~/project\x1b[0m$ ", "^C\r\n", "\x1b[1;32muser@host\x1b[0m:\x1b[1;34m~/project\x1b[0m$ "]
    return "".join(o).encode()


SYNTH = {
    "ink": g_ink, "ink-naive": g_ink_naive, "ratatui": g_ratatui, "alt-leave": g_alt_leave, "tmux-like": g_tmux,
    "fzf-like": g_fzf, "lazygit-like": g_lazygit, "btop-like": g_btop, "wide": g_wide, "sgr": g_sgr,
    "edit": g_edit, "sync": g_sync, "prompt": g_prompt,
}


def real_cases():
    d = workdir()
    tmp = tempfile.mkdtemp(prefix="dlconf-files-")
    open(os.path.join(tmp, "long.txt"), "w").write(LONG_TEXT)
    open(os.path.join(tmp, "app.py"), "w").write(PY_SOURCE)
    open(os.path.join(tmp, "curses_app.py"), "w").write(CURSES_APP)
    lsenv = {"LS_COLORS": "di=1;34:ln=1;36:ex=1;32:*.md=33:*.txt=35"}
    cases = {
        "ls-color": lambda c: run_pty(["ls", "--color=always", "-CF"], c, cwd=d, env=lsenv, wall=3),
        "less": lambda c: run_pty(["less", "-R", os.path.join(tmp, "long.txt")], c, keys=[(0.6, b" "), (1.0, b" "), (1.4, b"q")], wall=4, env={"LESS": ""}),
        "vim": lambda c: run_pty(["vim", "-u", "NONE", "-N", "-c", "syntax on | set number | set t_Co=256 | set background=dark", os.path.join(tmp, "app.py")], c,
                                 keys=[(1.0, b"\x06"), (1.5, b"/hello\r"), (2.0, b":q!\r")], wall=4),
        "vim-resize": lambda c: run_pty(["vim", "-u", "NONE", "-N", "-c", "syntax on | set number", os.path.join(tmp, "app.py")], c, rows=ROWS,
                                        resizes=[(1.0, max(c - 10, 20), ROWS - 6), (1.6, c, ROWS)], keys=[(2.4, b":q!\r")], wall=4),
        "nano": lambda c: run_pty(["nano", "--nonewlines", os.path.join(tmp, "app.py")], c, keys=[(1.0, b"\x16"), (1.6, b"\x18")], wall=4, env={"TERM": "xterm-256color"}),
        "htop": lambda c: run_pty(["htop", "-t"], c, keys=[(2.0, b"q")], wall=5, env={"HTOPRC": "/dev/null"}),
        "curses": lambda c: run_pty(["python3", os.path.join(tmp, "curses_app.py")], c, keys=[(0.8, b"q")], wall=3),
        "claude": lambda c: run_pty(["claude"], c, keys=[(5.0, b"\x03"), (5.4, b"\x03")], wall=7, env={"HOME": tempfile.mkdtemp(prefix="dlconf-home-"), "DISABLE_AUTOUPDATER": "1", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"}, netns=True),
        "codex": lambda c: run_pty(["codex"], c, keys=[(5.0, b"\x03"), (5.4, b"\x03")], wall=7, env={"HOME": tempfile.mkdtemp(prefix="dlconf-home-"), "CODEX_HOME": tempfile.mkdtemp(prefix="dlconf-codex-")}, netns=True),
    }
    return cases


def main():
    want = sys.argv[1:]
    for name, gen in SYNTH.items():
        if want and name not in want:
            continue
        for c in WIDTHS:
            open(os.path.join(HERE, "%s.%d.bin" % (name, c)), "wb").write(gen(c, ROWS))
    for name, run in real_cases().items():
        if want and name not in want:
            continue
        for c in WIDTHS:
            data = run(c)
            open(os.path.join(HERE, "%s.%d.bin" % (name, c)), "wb").write(data)
            side = os.path.join(HERE, "%s.%d.resize" % (name, c))
            if marks:
                open(side, "w").write("".join("%d %d %d\n" % m for m in marks))
            elif os.path.exists(side):
                os.remove(side)
            print(name, c, len(data), flush=True)


if __name__ == "__main__":
    main()
