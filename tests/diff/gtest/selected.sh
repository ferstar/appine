# Run the rows of gvisor.filters for one difftest group.
set -eu
dir=$(dirname "$0")
group=$1
found=0
failed=0
while read -r name program expected only skip; do
    [ "$name" = "$group" ] || continue
    found=1
    [ "$skip" != - ] || skip=""
    echo "@ $program"
    ONLY="$only" sh "$dir/run.sh" gvisor "$skip" "$program" </dev/null |
        awk -v program="$program" -v expected="$expected" '
            { print }
            NF == 2 && $2 == "ok" { passed++ }
            END {
                if (NR != expected || passed != expected) {
                    printf "%s: expected %d passes, got %d in %d lines\n", program, expected, passed, NR
                    exit 1
                }
            }' || failed=1
done < "$dir/gvisor.filters"
if [ "$found" = 0 ]; then
    echo "unknown gVisor selection: $group" >&2
    exit 1
fi
exit "$failed"
