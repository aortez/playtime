#!/bin/sh
set -eu
toolsets=",$1,"
apt-get update
apt-get install -y --no-install-recommends bash git ripgrep ca-certificates tar coreutils
case "$toolsets" in
    *,systems,*) apt-get install -y --no-install-recommends build-essential nodejs npm sqlite3 ;;
esac
case "$toolsets" in
    *,computation,*) python -m pip install --no-cache-dir --require-hashes -r /opt/workshop/requirements-computation.lock ;;
esac
case "$toolsets" in
    *,browser,*)
        python -m pip install --no-cache-dir --require-hashes -r /opt/workshop/requirements-browser.lock
        python -m playwright install --with-deps chromium
        chmod -R a+rX /opt/browsers
        ;;
esac
rm -rf /var/lib/apt/lists/*
printf '%s\n' "$1" > /opt/workshop/toolsets.txt
python -m pip freeze > /opt/workshop/python-packages.txt
dpkg-query -W > /opt/workshop/system-packages.txt
