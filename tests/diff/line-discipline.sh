# difftest-files: line-discipline
# The terminal line discipline against Linux's n_tty, scenario by scenario, using the scenarios and
# trace generator recorded for Starnix (tests/diff/line-discipline, see its README): each one drives
# a PTY pair through writes, reads, termios changes, packet mode, flushes, FIONREAD, poll and
# closes, and its trace is printed one event per line. A scenario that fails (a timeout, an
# unexpected errno) prints only the error's last line, so the others still run. Scenarios appine
# does not pass yet are skipped by group, each group a "Fidelity gaps" or bug item in the
# kernel/tty map; remove a group from SKIP once it is fixed.
cd /tmp/difftest-files/line-discipline
python3 - <<'PY'
import json
import subprocess
import sys

SKIP = {
    "ECHOPRT is not implemented": [
        "echo_prt", "echo_prt_two_chars", "echoprt_word_erase", "echoprt_word_erase_typing",
        "echoprt_word_erase_space", "echoprt_word_erase_tab", "echoprt_word_erase_nl_aln",
        "echoprt_word_erase_nl_nl", "echoprt_backspace_multi_aln",
        "echoprt_backspace_multi_space", "echoprt_backspace_multi_nl",
        "echoprt_backspace_nl_aln", "echoprt_backspace_nl_space", "echoprt_backspace_nl_nl",
        "echoprt_utf8_and_kill",
    ],
    "VREPRINT echoes with ECHO off": ["vreprint_no_echo_and_echoprt"],
    "ONOCR, ONLRET, XTABS and OLCUC are not implemented": [
        "output_onocr", "output_xtabs", "output_direct_onlret_onocr",
        "onlcr_resets_column_onocr", "olcuc_simple",
    ],
    "IUCLC, ISTRIP and PARMRK are not implemented": [
        "iuclc_simple", "istrip_basic", "parmrk_ff_doubling",
    ],
    "erase echo (tabs by column, VWERASE punctuation, no ECHOE)": [
        "tab_erase_columns", "werase_punctuation_and_no_echoe",
    ],
    "ECHOCTL of VEOL, VEOL2 and literal newlines; control erase without ECHOCTL": [
        "veol_basic", "veol2_basic", "noncanon_no_special_chars", "echoctl_special_chars",
    ],
    "TCOFLUSH on a slave drops output Linux has already passed on": [
        "tcoflush", "tcioflush", "tcioflush_canon",
    ],
    "packet mode has no flush or flow-control status bytes": [
        "ixon_flow_control_and_packet_mode", "signals_and_packet_mode",
    ],
    "EXTPROC does not disable local editing": ["extproc_basic"],
    "switching modes with pending input": ["icanon_toggle_state"],
}
skipped = {name: why for why, names in SKIP.items() for name in names}

for name in json.load(open("scenarios_list.json")):
    if name in skipped:
        print("==", name, "skipped:", skipped[name])
        continue
    out = "/tmp/ld-trace.json"
    r = subprocess.run(
        [sys.executable, "trace_generator.py", "--generate-trace",
         "--input", "scenarios/%s.json" % name, "--output", out],
        capture_output=True, text=True, timeout=300)
    print("==", name)
    if r.returncode != 0:
        lines = r.stderr.strip().splitlines()
        print("  error:", lines[-1] if lines else "exit %d" % r.returncode)
        continue
    for event in json.load(open(out))["events"]:
        print("  " + json.dumps(event, sort_keys=True))
PY
rm -f /tmp/ld-trace.json
