#!/bin/sh
# Mouse-reporting smoke test for ghostty-windows (run inside WSL).
#
#   GHOSTTY_WIN_SHELL="wsl.exe -e bash /mnt/c/Ghostty-windows/examples/mouse-test.sh" \
#       cargo run --release
#
# The terminal enables xterm mouse reporting (?1000 + SGR ?1006), then dumps
# whatever it receives with `cat -v`, so clicks appear as ^[[<0;col;rowM.

stty raw -echo
printf '\033[?1002h\033[?1006h'
printf 'mouse reporting on: click / drag / wheel in this window\r\n'
printf 'expected: ^[[<button;col;rowM\r\n\r\n'
cat -v
