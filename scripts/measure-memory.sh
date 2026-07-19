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
stress_steps=${NIVALIS_STRESS_STEPS:-}
test_case=${NIVALIS_MEMORY_TEST_CASE:-}

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
if [[ -n "$stress_steps" && "$stress_scenario" != "mixed" && "$stress_scenario" != "pagination" ]]; then
    printf 'Unsupported NIVALIS_STRESS_SCENARIO: %s\n' "$stress_scenario" >&2
    exit 1
fi
if [[ -n "$stress_steps" && "$stress_scenario" == "pagination" ]] && ((stress_steps % 2 != 0)); then
    printf 'Pagination stress requires an even NIVALIS_STRESS_STEPS value\n' >&2
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
if ((hard_gate)) && ((${#sample_points[@]} < 2)); then
    printf 'NIVALIS_MEMORY_HARD_GATE requires at least two samples for the growth check\n' >&2
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

printf 'test_case,renderer,platform,run,seconds,rss_kib,pss_kib,uss_kib,anonymous_kib,cpu_percent,vm_hwm_kib\n'

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
    previous_cpu_ticks=$(awk '{ print $14 + $15 }' "/proc/$pid/stat")
    previous_wall_ns=$(date +%s%N)
    previous=0
    baseline_rss_kib=0
    baseline_pss_kib=0
    settled_rss_kib=0
    settled_pss_kib=0
    for seconds in "${sample_points[@]}"; do
        delay=$((seconds - previous))
        if ((delay > 0)); then
            sleep "$delay"
        fi
        previous=$seconds

        if ! kill -0 "$pid" 2>/dev/null; then
            cat "$run_log_file" >&2
            exit 1
        fi

        current_cpu_ticks=$(awk '{ print $14 + $15 }' "/proc/$pid/stat")
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
            END {
                printf "%d %d %d %d", rss, pss, private_clean + private_dirty,
                    anonymous
            }
        ' "/proc/$pid/smaps_rollup")
        read -r rss_kib pss_kib uss_kib anonymous_kib <<<"$metrics"
        if ! is_bounded_positive_decimal "$rss_kib" 2147483647 ||
            ! is_bounded_positive_decimal "$pss_kib" 2147483647 ||
            ! is_bounded_nonnegative_decimal "$uss_kib" 2147483647 ||
            ! is_bounded_nonnegative_decimal "$anonymous_kib" 2147483647; then
            printf 'Could not read bounded smaps metrics for process %s: %s\n' \
                "$pid" "$metrics" >&2
            exit 1
        fi
        printf '%s,%s,%s,%d,%d,%d,%d,%d,%d,%s,%d\n' \
            "$test_case" "$renderer" "$platform" "$run" "$seconds" "$rss_kib" \
            "$pss_kib" "$uss_kib" "$anonymous_kib" "$cpu_percent" "$vm_hwm_kib"

        if ((baseline_rss_kib == 0)); then
            baseline_rss_kib=$rss_kib
            baseline_pss_kib=$pss_kib
        fi
        settled_rss_kib=$rss_kib
        settled_pss_kib=$pss_kib

        if ((hard_gate)) && ((rss_kib >= hard_cap_kib || vm_hwm_kib >= hard_cap_kib)); then
            printf 'Memory hard cap failed on run %d at %ss: RSS=%dKiB VmHWM=%dKiB; both must be below %dKiB\n' \
                "$run" "$seconds" "$rss_kib" "$vm_hwm_kib" "$hard_cap_kib" >&2
            exit 1
        fi
    done

    if ((hard_gate)); then
        growth_factor=$((100 + growth_limit_percent))
        if ((settled_rss_kib * 100 >= baseline_rss_kib * growth_factor ||
            settled_pss_kib * 100 >= baseline_pss_kib * growth_factor)); then
            printf 'Memory growth gate failed on run %d: baseline RSS/PSS=%d/%dKiB, settled=%d/%dKiB; growth must be below %d%%\n' \
                "$run" "$baseline_rss_kib" "$baseline_pss_kib" \
                "$settled_rss_kib" "$settled_pss_kib" "$growth_limit_percent" >&2
            exit 1
        fi
    fi

    if [[ -n "$stress_steps" ]]; then
        mapfile -t stress_errors < <(grep -E '^NIVALIS_STRESS_ERROR ' "$run_log_file" || true)
        if ((${#stress_errors[@]} > 0)); then
            printf 'Stress harness reported an error: %s\n' \
                "${stress_errors[${#stress_errors[@]} - 1]}" >&2
            exit 1
        fi
        mapfile -t stress_results < <(grep -E '^NIVALIS_STRESS_RESULT ' "$run_log_file" || true)
        if ((${#stress_results[@]} != 1)); then
            printf 'Stress harness must report exactly one completion marker; found %d\n' \
                "${#stress_results[@]}" >&2
            cat "$run_log_file" >&2
            exit 1
        fi
        stress_result=${stress_results[0]}
        if [[ "$stress_scenario" == "pagination" ]]; then
            pagination_pattern='^NIVALIS_STRESS_RESULT scenario=pagination transitions=([1-9][0-9]*) after=([1-9][0-9]*) before=([1-9][0-9]*) final_page=1 elapsed_ms=(0|[1-9][0-9]*)$'
            if [[ ! "$stress_result" =~ $pagination_pattern ]]; then
                printf 'Pagination stress completion marker has an invalid format: %s\n' \
                    "$stress_result" >&2
                exit 1
            fi
            half_steps=$((stress_steps / 2))
            if [[ "${BASH_REMATCH[1]}" != "$stress_steps" ||
                "${BASH_REMATCH[2]}" != "$half_steps" ||
                "${BASH_REMATCH[3]}" != "$half_steps" ]]; then
                printf 'Pagination stress completion counts do not match the requested transitions: %s\n' \
                    "$stress_result" >&2
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

    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    pid=""
    if [[ -n "$resize_pid" ]]; then
        wait "$resize_pid" 2>/dev/null || true
        resize_pid=""
    fi
    sleep 0.5
done
