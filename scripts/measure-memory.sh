#!/usr/bin/env bash

set -euo pipefail

binary=${1:-target/release/nivalis-mail}
renderer=${NIVALIS_RENDERER:-skia-software}
platform=${NIVALIS_MEMORY_PLATFORM:-x11}
width=${NIVALIS_MEMORY_WIDTH:-1200}
height=${NIVALIS_MEMORY_HEIGHT:-900}
sample_times=${NIVALIS_MEMORY_SAMPLES:-"1 3 5 10 20"}
runs=${NIVALIS_MEMORY_RUNS:-1}
resize_stress_width=${NIVALIS_RESIZE_STRESS_WIDTH:-0}
resize_stress_height=${NIVALIS_RESIZE_STRESS_HEIGHT:-0}
resize_stress_at=${NIVALIS_RESIZE_STRESS_AT:-5}
resize_stress_duration=${NIVALIS_RESIZE_STRESS_DURATION:-5}
log_file=${NIVALIS_MEMORY_LOG:-}
data_dir=${NIVALIS_MEMORY_DATA_DIR:-${NIVALIS_DATA_DIR:-}}
remove_log=0
remove_data_dir=0
hard_gate=${NIVALIS_MEMORY_HARD_GATE:-0}
hard_cap_kib=${NIVALIS_MEMORY_HARD_CAP_KIB:-92160}
growth_limit_percent=${NIVALIS_MEMORY_GROWTH_LIMIT_PERCENT:-100}
cpu_settle_gate=${NIVALIS_MEMORY_CPU_SETTLE_GATE:-$hard_gate}
cpu_settle_grace_seconds=${NIVALIS_MEMORY_CPU_SETTLE_GRACE_SECONDS:-5}
cpu_settle_seconds=${NIVALIS_MEMORY_CPU_SETTLE_SECONDS:-10}
cpu_settle_max_percent=${NIVALIS_MEMORY_CPU_SETTLE_MAX_PERCENT:-0.00}
stress_steps=${NIVALIS_STRESS_STEPS:-}
test_case=${NIVALIS_MEMORY_TEST_CASE:-}
account_diagnostic_delay_ms=${NIVALIS_STRESS_DELAY_MS:-5000}
account_receive_delay_ms=${NIVALIS_STRESS_DELAY_MS:-5000}
account_send_delay_ms=${NIVALIS_STRESS_DELAY_MS:-5000}
existing_account_sync_delay_ms=${NIVALIS_STRESS_DELAY_MS:-5000}

is_bounded_positive_decimal() {
    local value=$1
    local maximum=$2
    [[ "$value" =~ ^[1-9][0-9]*$ ]] || return 1
    ((${#value} < ${#maximum})) ||
        { ((${#value} == ${#maximum})) && [[ "$value" < "$maximum" || "$value" == "$maximum" ]]; }
}

is_bounded_nonnegative_decimal() {
    local value=$1
    local maximum=$2
    [[ "$value" == "0" ]] || is_bounded_positive_decimal "$value" "$maximum"
}

if [[ "$hard_gate" != "0" && "$hard_gate" != "1" ]]; then
    printf 'NIVALIS_MEMORY_HARD_GATE must be 0 or 1: %s\n' "$hard_gate" >&2
    exit 1
fi
if [[ "$cpu_settle_gate" != "0" && "$cpu_settle_gate" != "1" ]]; then
    printf 'NIVALIS_MEMORY_CPU_SETTLE_GATE must be 0 or 1: %s\n' "$cpu_settle_gate" >&2
    exit 1
fi
if ! is_bounded_nonnegative_decimal "$cpu_settle_grace_seconds" 2147483647; then
    printf 'NIVALIS_MEMORY_CPU_SETTLE_GRACE_SECONDS must be a non-negative integer: %s\n' \
        "$cpu_settle_grace_seconds" >&2
    exit 1
fi
if ! is_bounded_positive_decimal "$cpu_settle_seconds" 2147483647; then
    printf 'NIVALIS_MEMORY_CPU_SETTLE_SECONDS must be a positive integer: %s\n' \
        "$cpu_settle_seconds" >&2
    exit 1
fi
if [[ ! "$cpu_settle_max_percent" =~ ^[0-9]+([.][0-9]+)?$ ]] ||
    ! awk -v value="$cpu_settle_max_percent" 'BEGIN { exit !(value <= 100) }'; then
    printf 'NIVALIS_MEMORY_CPU_SETTLE_MAX_PERCENT must be between 0 and 100: %s\n' \
        "$cpu_settle_max_percent" >&2
    exit 1
fi
if ! is_bounded_positive_decimal "$runs" 2147483647; then
    printf 'NIVALIS_MEMORY_RUNS must be a positive decimal integer: %s\n' "$runs" >&2
    exit 1
fi
if ! is_bounded_positive_decimal "$hard_cap_kib" 2147483647; then
    printf 'NIVALIS_MEMORY_HARD_CAP_KIB must be a positive integer: %s\n' "$hard_cap_kib" >&2
    exit 1
fi
if ! is_bounded_nonnegative_decimal "$growth_limit_percent" 1000000; then
    printf 'NIVALIS_MEMORY_GROWTH_LIMIT_PERCENT must be a non-negative integer: %s\n' \
        "$growth_limit_percent" >&2
    exit 1
fi
if [[ -n "$stress_steps" ]] && ! is_bounded_positive_decimal "$stress_steps" 2147483647; then
    printf 'NIVALIS_STRESS_STEPS must be a positive integer: %s\n' "$stress_steps" >&2
    exit 1
fi
stress_scenario=${NIVALIS_STRESS_SCENARIO:-mixed}
if [[ -n "$stress_steps" && "$stress_scenario" != "mixed" &&
    "$stress_scenario" != "pagination" && "$stress_scenario" != "write-search" &&
    "$stress_scenario" != "content" && "$stress_scenario" != "account-diagnostic" &&
    "$stress_scenario" != "account-receive" && "$stress_scenario" != "account-send" &&
    "$stress_scenario" != "existing-account-sync" ]]; then
    printf 'Unsupported NIVALIS_STRESS_SCENARIO: %s\n' "$stress_scenario" >&2
    exit 1
fi
if [[ "$stress_scenario" == "account-diagnostic" && "$stress_steps" != "1" ]]; then
    printf 'account-diagnostic stress requires NIVALIS_STRESS_STEPS=1\n' >&2
    exit 1
fi
if [[ "$stress_scenario" == "account-receive" && "$stress_steps" != "1" ]]; then
    printf 'account-receive stress requires NIVALIS_STRESS_STEPS=1\n' >&2
    exit 1
fi
if [[ "$stress_scenario" == "account-send" && "$stress_steps" != "1" ]]; then
    printf 'account-send stress requires NIVALIS_STRESS_STEPS=1\n' >&2
    exit 1
fi
if [[ "$stress_scenario" == "existing-account-sync" && "$stress_steps" != "1" ]]; then
    printf 'existing-account-sync stress requires NIVALIS_STRESS_STEPS=1\n' >&2
    exit 1
fi
if [[ "$stress_scenario" == "account-diagnostic" && -z "$data_dir" ]]; then
    printf 'account-diagnostic stress requires an explicit persistent NIVALIS_MEMORY_DATA_DIR or NIVALIS_DATA_DIR\n' >&2
    exit 1
fi
if [[ "$stress_scenario" == "account-receive" && -z "$data_dir" ]]; then
    printf 'account-receive stress requires an explicit persistent NIVALIS_MEMORY_DATA_DIR or NIVALIS_DATA_DIR\n' >&2
    exit 1
fi
if [[ "$stress_scenario" == "account-send" && -z "$data_dir" ]]; then
    printf 'account-send stress requires an explicit persistent NIVALIS_MEMORY_DATA_DIR or NIVALIS_DATA_DIR\n' >&2
    exit 1
fi
if [[ "$stress_scenario" == "existing-account-sync" && -z "$data_dir" ]]; then
    printf 'existing-account-sync stress requires an explicit persistent NIVALIS_MEMORY_DATA_DIR or NIVALIS_DATA_DIR\n' >&2
    exit 1
fi
if [[ "$stress_scenario" == "existing-account-sync" ]]; then
    if [[ "${NIVALIS_STRESS_ALLOW_LIVE_SYNC:-}" != "1" ]]; then
        printf 'existing-account-sync stress requires NIVALIS_STRESS_ALLOW_LIVE_SYNC=1\n' >&2
        exit 1
    fi
    if ! is_bounded_positive_decimal \
        "${NIVALIS_STRESS_EXISTING_ACCOUNT_ID:-}" 9223372036854775807; then
        printf 'existing-account-sync stress requires a canonical positive account id\n' >&2
        exit 1
    fi
fi
if [[ "$stress_scenario" == "account-diagnostic" && "$cpu_settle_gate" != "1" ]]; then
    printf 'account-diagnostic stress requires NIVALIS_MEMORY_CPU_SETTLE_GATE=1\n' >&2
    exit 1
fi
if [[ "$stress_scenario" == "account-receive" && "$cpu_settle_gate" != "1" ]]; then
    printf 'account-receive stress requires NIVALIS_MEMORY_CPU_SETTLE_GATE=1\n' >&2
    exit 1
fi
if [[ "$stress_scenario" == "account-send" && "$cpu_settle_gate" != "1" ]]; then
    printf 'account-send stress requires NIVALIS_MEMORY_CPU_SETTLE_GATE=1\n' >&2
    exit 1
fi
if [[ "$stress_scenario" == "existing-account-sync" && "$cpu_settle_gate" != "1" ]]; then
    printf 'existing-account-sync stress requires NIVALIS_MEMORY_CPU_SETTLE_GATE=1\n' >&2
    exit 1
fi
if [[ "$stress_scenario" == "account-diagnostic" ]] &&
    ! is_bounded_positive_decimal "$account_diagnostic_delay_ms" 2147483647; then
    printf 'account-diagnostic stress requires a positive bounded NIVALIS_STRESS_DELAY_MS\n' >&2
    exit 1
fi
if [[ "$stress_scenario" == "account-receive" ]] &&
    ! is_bounded_positive_decimal "$account_receive_delay_ms" 2147483647; then
    printf 'account-receive stress requires a positive bounded NIVALIS_STRESS_DELAY_MS\n' >&2
    exit 1
fi
if [[ "$stress_scenario" == "account-send" ]] &&
    ! is_bounded_positive_decimal "$account_send_delay_ms" 2147483647; then
    printf 'account-send stress requires a positive bounded NIVALIS_STRESS_DELAY_MS\n' >&2
    exit 1
fi
if [[ "$stress_scenario" == "existing-account-sync" ]] &&
    ! is_bounded_positive_decimal "$existing_account_sync_delay_ms" 2147483647; then
    printf 'existing-account-sync stress requires a positive bounded NIVALIS_STRESS_DELAY_MS\n' >&2
    exit 1
fi
if [[ "$stress_scenario" == "account-send" ]]; then
    account_send_address=${NIVALIS_STRESS_ACCOUNT_ADDRESS:-}
    account_send_login=${NIVALIS_STRESS_ACCOUNT_LOGIN:-}
    account_send_imap_host=${NIVALIS_STRESS_ACCOUNT_IMAP_HOST:-}
    account_send_imap_port=${NIVALIS_STRESS_ACCOUNT_IMAP_PORT:-}
    account_send_smtp_host=${NIVALIS_STRESS_ACCOUNT_SMTP_HOST:-}
    account_send_smtp_port=${NIVALIS_STRESS_ACCOUNT_SMTP_PORT:-}
    account_send_secret_file=${NIVALIS_STRESS_ACCOUNT_SECRET_FILE:-}
    account_send_expected=${NIVALIS_STRESS_ACCOUNT_EXPECTED_RESULT:-}
    if [[ -z "$account_send_address" || -z "$account_send_login" ]]; then
        printf 'account-send stress requires explicit account address and login values\n' >&2
        exit 1
    fi
    if [[ "$account_send_imap_host" != "localhost" &&
        "$account_send_imap_host" != "127.0.0.1" &&
        "$account_send_imap_host" != "::1" ]]; then
        printf 'account-send stress requires an explicit loopback NIVALIS_STRESS_ACCOUNT_IMAP_HOST\n' >&2
        exit 1
    fi
    if ! is_bounded_positive_decimal "$account_send_imap_port" 65535; then
        printf 'account-send stress requires NIVALIS_STRESS_ACCOUNT_IMAP_PORT between 1 and 65535\n' >&2
        exit 1
    fi
    if [[ "$account_send_smtp_host" != "localhost" &&
        "$account_send_smtp_host" != "127.0.0.1" &&
        "$account_send_smtp_host" != "::1" ]]; then
        printf 'account-send stress requires an explicit loopback NIVALIS_STRESS_ACCOUNT_SMTP_HOST\n' >&2
        exit 1
    fi
    if ! is_bounded_positive_decimal "$account_send_smtp_port" 65535; then
        printf 'account-send stress requires NIVALIS_STRESS_ACCOUNT_SMTP_PORT between 1 and 65535\n' >&2
        exit 1
    fi
    if [[ "$account_send_secret_file" != /* || -L "$account_send_secret_file" ||
        ! -f "$account_send_secret_file" || ! -r "$account_send_secret_file" ]]; then
        printf 'account-send stress requires an absolute, readable, regular NIVALIS_STRESS_ACCOUNT_SECRET_FILE\n' >&2
        exit 1
    fi
    account_send_secret_mode=$(stat -c '%a' -- "$account_send_secret_file")
    if [[ ! "$account_send_secret_mode" =~ ^[0-7]+$ ]] ||
        ((8#$account_send_secret_mode & 077)); then
        printf 'account-send stress requires a secret file inaccessible to group and other users\n' >&2
        exit 1
    fi
    account_send_secret_bytes=$(wc -c <"$account_send_secret_file")
    if ! is_bounded_positive_decimal "$account_send_secret_bytes" 16384; then
        printf 'account-send stress requires a non-empty secret file no larger than 16384 bytes\n' >&2
        exit 1
    fi
    if [[ "$account_send_expected" != "ready" ]]; then
        printf 'account-send stress requires NIVALIS_STRESS_ACCOUNT_EXPECTED_RESULT=ready\n' >&2
        exit 1
    fi
    account_send_transition_timeout_ms=${NIVALIS_STRESS_TRANSITION_TIMEOUT_MS:-45000}
    if ! is_bounded_positive_decimal "$account_send_transition_timeout_ms" 2147483647; then
        printf 'account-send stress requires a positive bounded NIVALIS_STRESS_TRANSITION_TIMEOUT_MS\n' >&2
        exit 1
    fi
fi
if [[ -n "$stress_steps" &&
    ("$stress_scenario" == "pagination" || "$stress_scenario" == "write-search") ]] &&
    ((stress_steps % 2 != 0)); then
    printf '%s stress requires an even NIVALIS_STRESS_STEPS value\n' "$stress_scenario" >&2
    exit 1
fi
if [[ -z "$test_case" ]]; then
    if [[ -n "$stress_steps" ]]; then
        test_case="${stress_scenario}-stress"
    elif ((resize_stress_width > 0 && resize_stress_height > 0)); then
        test_case=resize
    else
        test_case=idle
    fi
fi
if [[ ! "$test_case" =~ ^[a-z0-9][a-z0-9._-]{0,63}$ ]]; then
    printf 'NIVALIS_MEMORY_TEST_CASE must be a lowercase CSV-safe identifier: %s\n' \
        "$test_case" >&2
    exit 1
fi

read -r -a sample_points <<<"$sample_times"
if ((${#sample_points[@]} == 0)); then
    printf 'NIVALIS_MEMORY_SAMPLES must contain at least one sample time\n' >&2
    exit 1
fi
previous_sample=0
for sample in "${sample_points[@]}"; do
    if ! is_bounded_positive_decimal "$sample" 2147483647 || ((sample <= previous_sample)); then
        printf 'NIVALIS_MEMORY_SAMPLES must be positive decimal integers in strictly increasing order: %s\n' \
            "$sample_times" >&2
        exit 1
    fi
    previous_sample=$sample
done
if [[ "$stress_scenario" == "account-diagnostic" ]] &&
    ((account_diagnostic_delay_ms <= sample_points[0] * 1000)); then
    printf 'account-diagnostic stress must start after the first memory baseline sample\n' >&2
    exit 1
fi
if [[ "$stress_scenario" == "account-receive" ]] &&
    ((account_receive_delay_ms <= sample_points[0] * 1000)); then
    printf 'account-receive stress must start after the first memory baseline sample\n' >&2
    exit 1
fi
if [[ "$stress_scenario" == "account-send" ]] &&
    ((account_send_delay_ms <= sample_points[0] * 1000)); then
    printf 'account-send stress must start after the first memory baseline sample\n' >&2
    exit 1
fi
if [[ "$stress_scenario" == "existing-account-sync" ]] &&
    ((existing_account_sync_delay_ms <= sample_points[0] * 1000)); then
    printf 'existing-account-sync stress must start after the first memory baseline sample\n' >&2
    exit 1
fi
if ((hard_gate)) && ((${#sample_points[@]} < 2)); then
    printf 'NIVALIS_MEMORY_HARD_GATE requires at least two samples for the growth check\n' >&2
    exit 1
fi
if ((cpu_settle_gate &&
    previous_sample + cpu_settle_grace_seconds + cpu_settle_seconds > 2147483647)); then
    printf 'CPU settle sample time exceeds the supported range\n' >&2
    exit 1
fi

if [[ ! -x "$binary" ]]; then
    printf 'Executable not found: %s\n' "$binary" >&2
    exit 1
fi

if [[ -z "$log_file" ]]; then
    log_file=$(mktemp)
    remove_log=1
else
    : >"$log_file"
fi
if [[ -z "$data_dir" ]]; then
    data_dir=$(mktemp -d)
    remove_data_dir=1
elif [[ "$data_dir" != /* ]]; then
    printf 'NIVALIS_MEMORY_DATA_DIR must be an absolute path: %s\n' "$data_dir" >&2
    exit 1
else
    mkdir -p "$data_dir"
fi
chmod 700 "$data_dir"

pid=""
resize_pid=""

cleanup() {
    if [[ -n "$resize_pid" ]] && kill -0 "$resize_pid" 2>/dev/null; then
        kill "$resize_pid" 2>/dev/null || true
        wait "$resize_pid" 2>/dev/null || true
    fi
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    fi
    if ((remove_log)); then
        rm -f "$log_file"
    fi
    if ((remove_data_dir)); then
        rm -rf -- "$data_dir"
    fi
}
trap cleanup EXIT INT TERM

read_process_stat() {
    local stat_line stat_tail
    local -a stat_fields

    if ! IFS= read -r stat_line <"/proc/$pid/stat"; then
        printf 'Could not read process statistics for PID %s\n' "$pid" >&2
        exit 1
    fi
    stat_tail=${stat_line##*) }
    read -r -a stat_fields <<<"$stat_tail"
    if [[ "$stat_tail" == "$stat_line" || ${#stat_fields[@]} -lt 20 ]]; then
        printf 'Malformed process statistics for PID %s\n' "$pid" >&2
        exit 1
    fi
    if [[ ! "${stat_fields[11]}" =~ ^[0-9]+$ ||
        ! "${stat_fields[12]}" =~ ^[0-9]+$ ||
        ! "${stat_fields[19]}" =~ ^[0-9]+$ ]]; then
        printf 'Malformed process statistics for PID %s\n' "$pid" >&2
        exit 1
    fi

    current_cpu_ticks=$((${stat_fields[11]} + ${stat_fields[12]}))
    current_start_ticks=${stat_fields[19]}
}

sample_process_metrics() {
    if ! kill -0 "$pid" 2>/dev/null; then
        cat "$run_log_file" >&2
        exit 1
    fi

    read_process_stat
    if [[ "$current_start_ticks" != "$process_start_ticks" ]]; then
        printf 'Process identity changed while measuring PID %s\n' "$pid" >&2
        exit 1
    fi
    current_wall_ns=$(date +%s%N)
    cpu_percent=$(awk \
        -v cpu_ticks="$((current_cpu_ticks - previous_cpu_ticks))" \
        -v wall_ns="$((current_wall_ns - previous_wall_ns))" \
        -v clock_ticks="$clock_ticks" \
        'BEGIN { printf "%.2f", 100 * cpu_ticks * 1000000000 / clock_ticks / wall_ns }')
    previous_cpu_ticks=$current_cpu_ticks
    previous_wall_ns=$current_wall_ns
    vm_hwm_kib=$(awk '$1 == "VmHWM:" { print $2; exit }' "/proc/$pid/status")
    if ! is_bounded_positive_decimal "$vm_hwm_kib" 2147483647; then
        printf 'Could not read a positive VmHWM value for process %s: %s\n' \
            "$pid" "$vm_hwm_kib" >&2
        exit 1
    fi
    metrics=$(awk '
        /^Rss:/ { rss = $2 }
        /^Pss:/ { pss = $2 }
        /^Private_Clean:/ { private_clean = $2 }
        /^Private_Dirty:/ { private_dirty = $2 }
        /^Anonymous:/ { anonymous = $2 }
        /^Swap:/ { swap = $2 }
        /^SwapPss:/ { swap_pss = $2 }
        END {
            printf "%d %d %d %d %d %d", rss, pss,
                private_clean + private_dirty, anonymous, swap, swap_pss
        }
    ' "/proc/$pid/smaps_rollup")
    read -r rss_kib pss_kib uss_kib anonymous_kib swap_kib swap_pss_kib <<<"$metrics"
    if ! is_bounded_positive_decimal "$rss_kib" 2147483647 ||
        ! is_bounded_positive_decimal "$pss_kib" 2147483647 ||
        ! is_bounded_nonnegative_decimal "$uss_kib" 2147483647 ||
        ! is_bounded_nonnegative_decimal "$anonymous_kib" 2147483647 ||
        ! is_bounded_nonnegative_decimal "$swap_kib" 2147483647 ||
        ! is_bounded_nonnegative_decimal "$swap_pss_kib" 2147483647; then
        printf 'Could not read bounded smaps metrics for process %s: %s\n' \
            "$pid" "$metrics" >&2
        exit 1
    fi
    if ((rss_kib > peak_rss_kib)); then
        peak_rss_kib=$rss_kib
    fi
    if ((vm_hwm_kib > peak_rss_kib)); then
        peak_rss_kib=$vm_hwm_kib
    fi
}

record_sample() {
    local seconds=$1
    printf '%s,%s,%s,%d,%d,%d,%d,%d,%d,%d,%d,%s,%d,%d\n' \
        "$test_case" "$renderer" "$platform" "$run" "$seconds" "$rss_kib" \
        "$pss_kib" "$uss_kib" "$anonymous_kib" "$swap_kib" "$swap_pss_kib" \
        "$cpu_percent" "$vm_hwm_kib" "$peak_rss_kib"

    if ((baseline_rss_kib == 0)); then
        baseline_rss_kib=$rss_kib
        baseline_pss_kib=$pss_kib
        baseline_total_rss_kib=$((rss_kib + swap_kib))
        baseline_total_pss_kib=$((pss_kib + swap_pss_kib))
    fi
    settled_rss_kib=$rss_kib
    settled_pss_kib=$pss_kib
    settled_total_rss_kib=$((rss_kib + swap_kib))
    settled_total_pss_kib=$((pss_kib + swap_pss_kib))

    if ((hard_gate)) && ((rss_kib >= hard_cap_kib || peak_rss_kib >= hard_cap_kib)); then
        printf 'Memory hard cap failed on run %d at %ss: RSS=%dKiB peak=%dKiB; both must be below %dKiB\n' \
            "$run" "$seconds" "$rss_kib" "$peak_rss_kib" "$hard_cap_kib" >&2
        exit 1
    fi
}

printf 'test_case,renderer,platform,run,seconds,rss_kib,pss_kib,uss_kib,anonymous_kib,swap_kib,swap_pss_kib,cpu_percent,vm_hwm_kib,peak_rss_kib\n'

for ((run = 1; run <= runs; run++)); do
    run_log_file=$log_file
    if ((runs > 1 && remove_log == 0)); then
        run_log_file="${log_file}.run-${run}"
    fi
    : >"$run_log_file"
    if [[ "$platform" == "x11" ]]; then
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET \
            WINIT_UNIX_BACKEND=x11 \
            SLINT_SCALE_FACTOR=1 \
            WINIT_X11_SCALE_FACTOR=1 \
            NIVALIS_RENDERER="$renderer" \
            NIVALIS_DATA_DIR="$data_dir" \
            "$binary" >"$run_log_file" 2>&1 &
    elif [[ "$platform" == "wayland" ]]; then
        env -u DISPLAY \
            WINIT_UNIX_BACKEND=wayland \
            XDG_SESSION_TYPE=wayland \
            SLINT_SCALE_FACTOR=1 \
            NIVALIS_RENDERER="$renderer" \
            NIVALIS_DATA_DIR="$data_dir" \
            "$binary" >"$run_log_file" 2>&1 &
    else
        printf 'Unsupported NIVALIS_MEMORY_PLATFORM: %s\n' "$platform" >&2
        exit 1
    fi
    pid=$!

    window_id=""
    if [[ "$platform" == "x11" ]]; then
        for _ in {1..50}; do
            if ! kill -0 "$pid" 2>/dev/null; then
                cat "$run_log_file" >&2
                exit 1
            fi
            window_id=$(xdotool search --pid "$pid" 2>/dev/null | head -n 1 || true)
            if [[ -n "$window_id" ]]; then
                break
            fi
            sleep 0.1
        done
        if [[ -z "$window_id" ]]; then
            printf 'Could not find the X11 window for process %s\n' "$pid" >&2
            cat "$run_log_file" >&2
            exit 1
        fi
    fi

    if [[ -n "$window_id" ]]; then
        xdotool windowsize "$window_id" "$width" "$height"
        geometry_matches=0
        for _ in {1..50}; do
            geometry=$(xdotool getwindowgeometry --shell "$window_id")
            actual_width=$(awk -F= '$1 == "WIDTH" { print $2 }' <<<"$geometry")
            actual_height=$(awk -F= '$1 == "HEIGHT" { print $2 }' <<<"$geometry")
            if [[ "$actual_width" == "$width" && "$actual_height" == "$height" ]]; then
                geometry_matches=1
                break
            fi
            sleep 0.1
        done
        if ((geometry_matches == 0)); then
            printf 'X11 window did not reach %sx%s; last geometry was %sx%s\n' \
                "$width" "$height" "$actual_width" "$actual_height" >&2
            exit 1
        fi
        if ((resize_stress_width > 0 && resize_stress_height > 0)); then
            (
                sleep "$resize_stress_at"
                xdotool windowsize "$window_id" "$resize_stress_width" "$resize_stress_height"
                sleep "$resize_stress_duration"
                xdotool windowsize "$window_id" "$width" "$height"
            ) &
            resize_pid=$!
        fi
    fi

    clock_ticks=$(getconf CLK_TCK)
    read_process_stat
    previous_cpu_ticks=$current_cpu_ticks
    process_start_ticks=$current_start_ticks
    previous_wall_ns=$(date +%s%N)
    previous=0
    peak_rss_kib=0
    baseline_rss_kib=0
    baseline_pss_kib=0
    baseline_total_rss_kib=0
    baseline_total_pss_kib=0
    settled_rss_kib=0
    settled_pss_kib=0
    settled_total_rss_kib=0
    settled_total_pss_kib=0
    for seconds in "${sample_points[@]}"; do
        delay=$((seconds - previous))
        if ((delay > 0)); then
            sleep "$delay"
        fi
        previous=$seconds

        sample_process_metrics
        record_sample "$seconds"
    done

    if [[ -n "$stress_steps" ]]; then
        mapfile -t stress_errors < <(grep -E '^NIVALIS_STRESS_ERROR ' "$run_log_file" || true)
        if ((${#stress_errors[@]} > 0)); then
            printf 'Stress harness reported an error: %s\n' \
                "${stress_errors[${#stress_errors[@]} - 1]}" >&2
            if [[ "$stress_scenario" == "account-diagnostic" &&
                "${stress_errors[${#stress_errors[@]} - 1]}" == *' cleanup_required=1' ]]; then
                printf 'Account diagnostic recovery data retained at %s\n' "$data_dir" >&2
            fi
            if [[ "$stress_scenario" == "account-receive" ]]; then
                printf 'Account receive recovery data retained at %s\n' "$data_dir" >&2
            fi
            if [[ "$stress_scenario" == "account-send" ]]; then
                printf 'Account send recovery data retained at %s\n' "$data_dir" >&2
            fi
            exit 1
        fi
        mapfile -t stress_results < <(grep -E '^NIVALIS_STRESS_RESULT ' "$run_log_file" || true)
        if ((${#stress_results[@]} != 1)); then
            printf 'Stress harness must report exactly one completion marker; found %d\n' \
                "${#stress_results[@]}" >&2
            cat "$run_log_file" >&2
            if [[ "$stress_scenario" == "account-receive" ]]; then
                printf 'Account receive recovery data retained at %s\n' "$data_dir" >&2
            fi
            if [[ "$stress_scenario" == "account-send" ]]; then
                printf 'Account send recovery data retained at %s\n' "$data_dir" >&2
            fi
            exit 1
        fi
        stress_result=${stress_results[0]}
        if [[ "$stress_scenario" == "account-send" ]]; then
            account_send_pattern='^NIVALIS_STRESS_RESULT scenario=account-send steps=1 queued=1 delivered=1 sent_visible=1 drafts=0 removed=1 elapsed_ms=(0|[1-9][0-9]*)$'
            if [[ ! "$stress_result" =~ $account_send_pattern ]]; then
                printf 'Account-send stress completion marker has an invalid format: %s\n' \
                    "$stress_result" >&2
                printf 'Account send recovery data retained at %s\n' "$data_dir" >&2
                exit 1
            fi
        elif [[ "$stress_scenario" == "existing-account-sync" ]]; then
            existing_account_sync_pattern='^NIVALIS_STRESS_RESULT scenario=existing-account-sync steps=1 manual_sync=1 timestamp=1 database=1 ui=1 elapsed_ms=(0|[1-9][0-9]*)$'
            if [[ ! "$stress_result" =~ $existing_account_sync_pattern ]]; then
                printf 'Existing-account-sync completion marker has an invalid format: %s\n' \
                    "$stress_result" >&2
                exit 1
            fi
        elif [[ "$stress_scenario" == "account-receive" ]]; then
            account_receive_pattern='^NIVALIS_STRESS_RESULT scenario=account-receive steps=1 manual_sync=1 database=1 ui=1 reader=1 imported=1 opened=1 closed=1 removed=1 elapsed_ms=(0|[1-9][0-9]*)$'
            if [[ ! "$stress_result" =~ $account_receive_pattern ]]; then
                printf 'Account-receive stress completion marker has an invalid format: %s\n' \
                    "$stress_result" >&2
                printf 'Account receive recovery data retained at %s\n' "$data_dir" >&2
                exit 1
            fi
        elif [[ "$stress_scenario" == "account-diagnostic" ]]; then
            account_diagnostic_pattern='^NIVALIS_STRESS_RESULT scenario=account-diagnostic cycles=1 outcome=ready removed=1 elapsed_ms=(0|[1-9][0-9]*)$'
            if [[ ! "$stress_result" =~ $account_diagnostic_pattern ]]; then
                printf 'Account-diagnostic stress completion marker has an invalid format: %s\n' \
                    "$stress_result" >&2
                exit 1
            fi
        elif [[ "$stress_scenario" == "pagination" ]]; then
            pagination_pattern='^NIVALIS_STRESS_RESULT scenario=pagination transitions=([1-9][0-9]*) first=([1-9][0-9]*) after=([1-9][0-9]*) before=0 final_rows=50 elapsed_ms=(0|[1-9][0-9]*)$'
            if [[ ! "$stress_result" =~ $pagination_pattern ]]; then
                printf 'Pagination stress completion marker has an invalid format: %s\n' \
                    "$stress_result" >&2
                exit 1
            fi
            half_steps=$((stress_steps / 2))
            if [[ "${BASH_REMATCH[1]}" != "$stress_steps" ||
                "${BASH_REMATCH[2]}" != "$half_steps" ||
                "${BASH_REMATCH[3]}" != "$half_steps" ]]; then
                printf 'Waterfall stress completion counts do not match the requested transitions: %s\n' \
                    "$stress_result" >&2
                exit 1
            fi
        elif [[ "$stress_scenario" == "write-search" ]]; then
            write_search_pattern='^NIVALIS_STRESS_RESULT scenario=write-search cycles=([1-9][0-9]*) writes=([1-9][0-9]*) searches=([1-9][0-9]*) clears=([1-9][0-9]*) first_queries=([1-9][0-9]*) after_queries=0 before_queries=0 target_id=51 final_page=1 final_query=empty final_starred=(true|false) elapsed_ms=(0|[1-9][0-9]*)$'
            if [[ ! "$stress_result" =~ $write_search_pattern ]]; then
                printf 'Write-search stress completion marker has an invalid format: %s\n' \
                    "$stress_result" >&2
                exit 1
            fi
            expected_first_queries=$((stress_steps * 3))
            if [[ "${BASH_REMATCH[1]}" != "$stress_steps" ||
                "${BASH_REMATCH[2]}" != "$stress_steps" ||
                "${BASH_REMATCH[3]}" != "$stress_steps" ||
                "${BASH_REMATCH[4]}" != "$stress_steps" ||
                "${BASH_REMATCH[5]}" != "$expected_first_queries" ]]; then
                printf 'Write-search stress completion counts do not match the requested cycles: %s\n' \
                    "$stress_result" >&2
                exit 1
            fi
        elif [[ "$stress_scenario" == "content" ]]; then
            content_pattern='^NIVALIS_STRESS_RESULT scenario=content cycles=([1-9][0-9]*) imports=([1-9][0-9]*) body_opens=([1-9][0-9]*) attachment_opens=([1-9][0-9]*) gc_runs=([1-9][0-9]*) gc_examined=([1-9][0-9]*) gc_removed=(0|[1-9][0-9]*) gc_missing=(0|[1-9][0-9]*) files_per_import=2 target_id=51 elapsed_ms=(0|[1-9][0-9]*)$'
            if [[ ! "$stress_result" =~ $content_pattern ]]; then
                printf 'Content stress completion marker has an invalid format: %s\n' \
                    "$stress_result" >&2
                exit 1
            fi
            if [[ "${BASH_REMATCH[1]}" != "$stress_steps" ||
                "${BASH_REMATCH[2]}" != "$stress_steps" ||
                "${BASH_REMATCH[3]}" != "$stress_steps" ||
                "${BASH_REMATCH[4]}" != "$stress_steps" ||
                "${BASH_REMATCH[5]}" != "$stress_steps" ]]; then
                printf 'Content stress completion counts do not match the requested cycles: %s\n' \
                    "$stress_result" >&2
                exit 1
            fi
            gc_examined=${BASH_REMATCH[6]}
            gc_removed=${BASH_REMATCH[7]}
            gc_missing=${BASH_REMATCH[8]}
            minimum_gc=$((2 * (stress_steps - 1) + 1))
            maximum_gc=$((2 * stress_steps))
            if ((gc_examined != gc_removed + gc_missing ||
                gc_examined < minimum_gc || gc_examined > maximum_gc)); then
                printf 'Content stress GC counts are inconsistent: %s\n' "$stress_result" >&2
                exit 1
            fi
        else
            mixed_pattern='^NIVALIS_STRESS_RESULT scenario=mixed steps=([1-9][0-9]*) elapsed_ms=(0|[1-9][0-9]*)$'
            if [[ ! "$stress_result" =~ $mixed_pattern || "${BASH_REMATCH[1]}" != "$stress_steps" ]]; then
                printf 'Mixed stress completion marker does not match the requested steps: %s\n' \
                    "$stress_result" >&2
                exit 1
            fi
        fi
        printf '%s\n' "$stress_result" >&2
    fi

    if ((cpu_settle_gate)); then
        if ((cpu_settle_grace_seconds > 0)); then
            sleep "$cpu_settle_grace_seconds"
        fi
        read_process_stat
        if [[ "$current_start_ticks" != "$process_start_ticks" ]]; then
            printf 'Process identity changed while measuring PID %s\n' "$pid" >&2
            exit 1
        fi
        previous_cpu_ticks=$current_cpu_ticks
        previous_wall_ns=$(date +%s%N)
        sleep "$cpu_settle_seconds"
        settled_seconds=$((previous + cpu_settle_grace_seconds + cpu_settle_seconds))
        sample_process_metrics
        record_sample "$settled_seconds"
        if ! awk -v actual="$cpu_percent" -v maximum="$cpu_settle_max_percent" \
            'BEGIN { exit !(actual <= maximum) }'; then
            printf 'CPU settle gate failed on run %d: %s%% over %ss exceeds %s%%\n' \
                "$run" "$cpu_percent" "$cpu_settle_seconds" \
                "$cpu_settle_max_percent" >&2
            exit 1
        fi
    fi

    if ((hard_gate)); then
        growth_factor=$((100 + growth_limit_percent))
        if ((settled_rss_kib * 100 >= baseline_rss_kib * growth_factor ||
            settled_pss_kib * 100 >= baseline_pss_kib * growth_factor ||
            settled_total_rss_kib * 100 >= baseline_total_rss_kib * growth_factor ||
            settled_total_pss_kib * 100 >= baseline_total_pss_kib * growth_factor)); then
            printf 'Memory growth gate failed on run %d: baseline RSS/PSS/total-RSS/total-PSS=%d/%d/%d/%dKiB, settled=%d/%d/%d/%dKiB; growth must be below %d%%\n' \
                "$run" "$baseline_rss_kib" "$baseline_pss_kib" \
                "$baseline_total_rss_kib" "$baseline_total_pss_kib" \
                "$settled_rss_kib" "$settled_pss_kib" "$settled_total_rss_kib" \
                "$settled_total_pss_kib" "$growth_limit_percent" >&2
            exit 1
        fi
    fi

    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    pid=""
    if [[ -n "$resize_pid" ]]; then
        wait "$resize_pid" 2>/dev/null || true
        resize_pid=""
    fi
    sleep 0.5
done
