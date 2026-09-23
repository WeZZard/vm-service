# `vmctl --help` golden corpus

Byte-for-byte reference output from the Python implementation this crate
replaced. The Rust `vmctl` renders `--help` through an `argparse` formatter
port (`src/argparse_help.rs`), and `tests/help_golden.rs` compares its output
against these files at widths 20, 24, 25, 30, 33, 40, 80, 120, 160, 240 and
300. The narrow widths cross the minimum help position `argparse` will use, and
the wide widths go past the point where wrapping stops changing.

Each directory is a `COLUMNS` value. `top.txt` is the top-level parser; every
other file is the same-named subcommand. The corpus covers the top level and
all 18 subcommands (19 invocations) at each width.

## Provenance

Generated from `bin/vmctl` at commit `193e0f6`, extracted to `/tmp/py-vmctl`,
with CPython 3.14.7. The exact generation command run from `/tmp/py-vmctl`,
writing into this directory:

```sh
commands=(environment acquire wait list images images-show status exec push pull \
  heartbeat release capabilities acquisition-capabilities console-resolve \
  console-open console-cancel gc)
for w in 20 24 25 30 33 40 80 120 160 240 300; do
  COLUMNS=$w python3 vmctl --help > "<corpus>/$w/top.txt"
  for c in "${commands[@]}"; do
    COLUMNS=$w python3 vmctl "$c" --help > "<corpus>/$w/$c.txt"
  done
done
```

To regenerate at an arbitrary width, for example 80:

```sh
cd /tmp/py-vmctl
COLUMNS=80 python3 vmctl --help
COLUMNS=80 python3 vmctl acquire --help
```

All captures are non-tty (`stdout` redirected to a file), so no ANSI escapes
are present.
