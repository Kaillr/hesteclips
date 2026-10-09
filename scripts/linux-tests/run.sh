#!/usr/bin/env bash
# Run the Linux tests in containers: real Linux desktops (X11, Sway, GNOME,
# KDE) and sound (PipeWire), on any machine with Docker — a Mac too.
#
#   scripts/linux-tests/run.sh            all of them
#   scripts/linux-tests/run.sh sound-sync x11-game     just these
#
# Each test prints what it found; read it (there's no pass/fail count yet).
# Pictures from the capture tests go to target/linux-tests/. There's no GPU in
# the containers: KDE's screen stream, hardware encoders and GPU decoding
# can't be tested here.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="$root/target/linux-tests"
mkdir -p "$out"

# Which machine each test runs on.
machine() {
    case "$1" in
        gnome-*) echo gnome ;;
        kde-*) echo kde ;;
        *) echo ubuntu ;;
    esac
}

tests=("$@")
if [ ${#tests[@]} -eq 0 ]; then
    tests=($(cd "$here" && ls *.sh | grep -v '^run\.sh$' | sed 's/\.sh$//'))
fi

for t in "${tests[@]}"; do
    m=$(machine "$t")
    docker build -q -t "hesteclips-test-$m" -f "$here/$m.Dockerfile" "$here" >/dev/null || { echo "couldn't build the $m machine"; exit 1; }
    echo "=== $t (on $m)"
    # Builds go to a volume per machine, so each test doesn't rebuild everything.
    docker run --rm \
        -v "$root":/src -v "$out":/out -v "$here/$t.sh":/test.sh \
        -v "hesteclips-test-target-$m":/target -v "hesteclips-test-cargo":/root/.cargo/registry \
        -e CARGO_TARGET_DIR=/target -e GRANT=1 -e TRAY=1 \
        "hesteclips-test-$m" bash /test.sh 2>&1 | grep -vE 'pw\.conf|conf\.c|PipeWire: Creation failed'
done
