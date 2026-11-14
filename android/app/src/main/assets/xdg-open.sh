#!/bin/sh
# Appine's launcher reads the current bridge endpoint on every invocation, including in tmux.
exec /usr/local/libexec/appine-open-url "$@"
