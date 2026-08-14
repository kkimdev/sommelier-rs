#!/usr/bin/env bash
#
# Run the standalone IME GUI through a locally built sommelier --gpu-accel
# instance.  This is intentionally opt-in: it needs a live Wayland session and
# either /dev/wl0 or an explicitly selected local compositor.
#
# Examples:
#   scripts/sommelier-gui-smoke.sh
#   SOMMELIER_GUI_SMOKE_COMPOSITOR=/run/user/1000/wayland-0 \
#     scripts/sommelier-gui-smoke.sh
#   SOMMELIER_GUI_SMOKE_REQUIRE_GPU=1 scripts/sommelier-gui-smoke.sh

set -Eeuo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/sommelier-gui-smoke.sh

Environment:
  SOMMELIER_BIN              Proxy executable (auto-detected under target/).
  SOMMELIER_TEST_GUI_BIN     GUI executable (auto-detected under target/).
  SOMMELIER_GUI_SMOKE_VIRTWL VirtWL device, default /dev/wl0.
  SOMMELIER_GUI_SMOKE_COMPOSITOR
                             Local compositor socket instead of VirtWL.
  SOMMELIER_GUI_SMOKE_DISPLAY
                             Wayland display name for the temporary proxy.
  SOMMELIER_GUI_SMOKE_RUST_LOG
                             Proxy log filter; defaults to SHM/transport debug.
  SOMMELIER_GUI_SMOKE_APP_LOG
                             GUI log filter; defaults to info.
  SOMMELIER_GUI_SMOKE_TIMEOUT
                             Maximum GUI runtime in seconds; defaults to 15.
  SOMMELIER_GUI_SMOKE_REQUIRE_GPU
                             Set to 1 to fail unless a linux-dmabuf allocation
                             succeeds.  The default accepts the validated SHM
                             fallback when the kernel reports no dma-buf ioctl.
  SOMMELIER_GUI_SMOKE_KEEP_LOGS
                             Set to 1 to retain logs after a successful run.
EOF
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi
if [[ $# -ne 0 ]]; then
    echo "error: this script accepts configuration through environment variables" >&2
    usage >&2
    exit 2
fi

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
runtime_dir="${XDG_RUNTIME_DIR:-/run/user/${UID}}"
display="${SOMMELIER_GUI_SMOKE_DISPLAY:-wayland-sommelier-gui-${PPID}-${BASHPID}}"
socket="${runtime_dir}/${display}"
log_dir="$(mktemp -d "${TMPDIR:-/tmp}/sommelier-gui-smoke.XXXXXX")"
proxy_log="${log_dir}/proxy.log"
app_log="${log_dir}/app.log"
proxy_pid=""
keep_logs="${SOMMELIER_GUI_SMOKE_KEEP_LOGS:-0}"
gui_timeout="${SOMMELIER_GUI_SMOKE_TIMEOUT:-15}"

find_binary() {
    local override="$1"
    shift
    if [[ -n "$override" ]]; then
        printf '%s\n' "$override"
        return 0
    fi

    local candidate
    for candidate in "$@"; do
        if [[ -x "$candidate" ]]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

proxy_bin="$(find_binary "${SOMMELIER_BIN:-}" \
    "${repo_root}/target/release/sommelier" \
    "${repo_root}/target/x86_64-unknown-linux-gnu/release/sommelier" \
    "${repo_root}/target/debug/sommelier" \
    "${repo_root}/target/x86_64-unknown-linux-gnu/debug/sommelier")" || {
    echo "error: sommelier binary not found; build -p sommelier first" >&2
    exit 1
}
gui_bin="$(find_binary "${SOMMELIER_TEST_GUI_BIN:-}" \
    "${repo_root}/target/release/sommelier-test-gui" \
    "${repo_root}/target/x86_64-unknown-linux-gnu/release/sommelier-test-gui" \
    "${repo_root}/target/debug/sommelier-test-gui" \
    "${repo_root}/target/x86_64-unknown-linux-gnu/debug/sommelier-test-gui")" || {
    echo "error: sommelier-test-gui binary not found; build -p sommelier-test-gui first" >&2
    exit 1
}

if [[ ! -d "$runtime_dir" ]]; then
    echo "error: XDG runtime directory does not exist: ${runtime_dir}" >&2
    exit 1
fi
if [[ -e "$socket" ]]; then
    echo "error: temporary Wayland socket already exists: ${socket}" >&2
    echo "       choose SOMMELIER_GUI_SMOKE_DISPLAY or remove the stale socket" >&2
    exit 1
fi

print_logs() {
    echo "--- proxy log (${proxy_log}) ---" >&2
    sed -n '1,240p' "$proxy_log" >&2 || true
    echo "--- GUI log (${app_log}) ---" >&2
    sed -n '1,240p' "$app_log" >&2 || true
}

cleanup() {
    local status=$?
    trap - EXIT INT TERM
    if [[ -n "$proxy_pid" ]] && kill -0 "$proxy_pid" 2>/dev/null; then
        kill "$proxy_pid" 2>/dev/null || true
        wait "$proxy_pid" 2>/dev/null || true
    fi
    if [[ -e "$socket" ]]; then
        rm -f -- "$socket" "${socket}.lock"
    fi
    if (( status != 0 )); then
        print_logs
    fi
    if [[ "$keep_logs" == "1" ]]; then
        echo "sommelier GUI smoke logs: ${log_dir}" >&2
    else
        rm -rf -- "$log_dir"
    fi
    exit "$status"
}
trap cleanup EXIT INT TERM

virtwl_device="${SOMMELIER_GUI_SMOKE_VIRTWL:-/dev/wl0}"
if [[ -n "${SOMMELIER_GUI_SMOKE_COMPOSITOR:-}" ]]; then
    proxy_args=(
        --local-compositor "${SOMMELIER_GUI_SMOKE_COMPOSITOR}"
        --gpu-accel "$display"
    )
    transport_label="local compositor"
else
    if [[ ! -e "$virtwl_device" ]]; then
        echo "error: VirtWL device does not exist: ${virtwl_device}" >&2
        exit 1
    fi
    proxy_args=(
        --virtio-wl "$virtwl_device"
        --gpu-accel "$display"
    )
    transport_label="VirtWL ${virtwl_device}"
fi

proxy_log_filter="${SOMMELIER_GUI_SMOKE_RUST_LOG:-sommelier::proxy=info,sommelier::handler::shm=debug,sommelier::virtwl_channel=debug,sommelier::handler::keyboard=debug,sommelier::handler::text_input=debug}"
app_log_filter="${SOMMELIER_GUI_SMOKE_APP_LOG:-info}"

echo "Starting ${proxy_bin} --gpu-accel (${transport_label})"
env RUST_LOG="$proxy_log_filter" XDG_RUNTIME_DIR="$runtime_dir" \
    "$proxy_bin" "${proxy_args[@]}" >"$proxy_log" 2>&1 &
proxy_pid=$!

for _ in {1..100}; do
    if [[ -S "$socket" ]]; then
        break
    fi
    if ! kill -0 "$proxy_pid" 2>/dev/null; then
        echo "error: sommelier exited before creating ${socket}" >&2
        exit 1
    fi
    sleep 0.05
done
if [[ ! -S "$socket" ]]; then
    echo "error: timed out waiting for ${socket}" >&2
    exit 1
fi

set +e
env RUST_LOG="$app_log_filter" WAYLAND_DISPLAY="$display" \
    XDG_RUNTIME_DIR="$runtime_dir" \
    timeout --signal=TERM "$gui_timeout" "$gui_bin" --auto-exit >"$app_log" 2>&1
app_status=$?
set -e
if (( app_status != 0 )); then
    if (( app_status == 124 )); then
        echo "error: sample GUI exceeded SOMMELIER_GUI_SMOKE_TIMEOUT=${gui_timeout}s; inspect the retained logs for frame/input progress" >&2
    fi
    echo "error: sample GUI exited with status ${app_status}" >&2
    exit "$app_status"
fi

grep -Fq "Starting Sommelier IME Test Application" "$app_log" || {
    echo "error: sample GUI did not start cleanly" >&2
    exit 1
}
grep -Fq "Registered Korean IME test font" "$app_log" || {
    echo "error: sample GUI did not initialize its Korean test font" >&2
    exit 1
}
grep -Fq "[OnExit] Application closing." "$app_log" || {
    echo "error: sample GUI did not report a clean close" >&2
    exit 1
}
grep -Fq "New client connected" "$proxy_log" || {
    echo "error: proxy did not observe a GUI client connection" >&2
    exit 1
}
grep -Eq "text_input|XKB keymap" "$proxy_log" || {
    echo "error: proxy did not complete the keyboard/text-input handshake" >&2
    exit 1
}

transport="unknown"
if grep -Fq "Using VirtWL linux-dmabuf allocation" "$proxy_log"; then
    transport="virtwl-linux-dmabuf"
elif grep -Fq "Using GBM linux-dmabuf allocation" "$proxy_log"; then
    transport="gbm-linux-dmabuf"
elif grep -Fq "Using VirtWL shared-memory allocation" "$proxy_log" \
    || grep -Fq "Copied damaged SHM buffer" "$proxy_log"; then
    transport="validated-shm-fallback"
fi
if [[ "$transport" == "unknown" ]]; then
    echo "error: proxy never selected a recognized buffer transport" >&2
    exit 1
fi
if [[ "${SOMMELIER_GUI_SMOKE_REQUIRE_GPU:-0}" == "1" \
    && "$transport" != "virtwl-linux-dmabuf" \
    && "$transport" != "gbm-linux-dmabuf" ]]; then
    echo "error: GPU transport was required, selected ${transport}" >&2
    exit 1
fi

echo "sommelier GUI smoke passed: transport=${transport}, input=text-input/keymap"
