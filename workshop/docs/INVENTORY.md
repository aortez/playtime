# Workshop

Your writable, persistent workspace is `/work`. Only its files survive a
session reset. Processes and `/tmp` are reset. There is no external network.

Installed collections are listed in `/opt/workshop/toolsets.txt`.

- **core**: Python, Bash, Git, ripgrep, standard Unix tools. Write and execute
  programs, build your own helpers, and inspect their output.
- **computation** (when installed): NumPy, SciPy, Matplotlib, Pillow. Numerical
  work, simulations, plots, and image processing. See `computation.md` here.
- **browser** (when installed): Playwright and Chromium. Exercise local web
  applications, inspect their DOM, and capture images. See `browser.md` here.
- **systems** (when installed): C/C++ compiler, Make, Node.js, npm, SQLite.
  Build executables, JavaScript programs, and small databases.

Exact installed versions are in `/opt/workshop/python-packages.txt` and
`/opt/workshop/system-packages.txt`. You can read these files with `exec`.
Offline package installation is limited to materials already in the image.

Images and audio you save become artifacts for the human reviewer. The initial
model interface returns text; it does not provide image or audio perception.
Use numerical checks, DOM inspection, and other text observations as needed.

Shell commands have a timeout and an output limit. Save longer output in files
and read relevant portions. Background processes may run until the end of this
session. Redirect their output to files and save useful state before the reset.
