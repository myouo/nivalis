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

printf 'renderer,platform,run,seconds,rss_kib,pss_kib,uss_kib,anonymous_kib,cpu_percent\n'

for ((run = 1; run <= runs; run++)); do
    if [[ "$platform" == "x11" ]]; then
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET \
            WINIT_UNIX_BACKEND=x11 \
            SLINT_SCALE_FACTOR=1 \
            WINIT_X11_SCALE_FACTOR=1 \
            NIVALIS_RENDERER="$renderer" \
            NIVALIS_DATA_DIR="$data_dir" \
            "$binary" >"$log_file" 2>&1 &
    elif [[ "$platform" == "wayland" ]]; then
        env -u DISPLAY \
            WINIT_UNIX_BACKEND=wayland \
            XDG_SESSION_TYPE=wayland \
            SLINT_SCALE_FACTOR=1 \
            NIVALIS_RENDERER="$renderer" \
            NIVALIS_DATA_DIR="$data_dir" \
            "$binary" >"$log_file" 2>&1 &
    else
        printf 'Unsupported NIVALIS_MEMORY_PLATFORM: %s\n' "$platform" >&2
        exit 1
    fi
    pid=$!

    window_id=""
    if [[ "$platform" == "x11" ]]; then
        for _ in {1..50}; do
            if ! kill -0 "$pid" 2>/dev/null; then
                cat "$log_file" >&2
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
            cat "$log_file" >&2
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
    for seconds in $sample_times; do
        delay=$((seconds - previous))
        if ((delay > 0)); then
            sleep "$delay"
        fi
        previous=$seconds

        if ! kill -0 "$pid" 2>/dev/null; then
            cat "$log_file" >&2
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
        awk -v renderer="$renderer" -v platform="$platform" -v run="$run" \
            -v seconds="$seconds" -v cpu_percent="$cpu_percent" '
            /^Rss:/ { rss = $2 }
            /^Pss:/ { pss = $2 }
            /^Private_Clean:/ { private_clean = $2 }
            /^Private_Dirty:/ { private_dirty = $2 }
            /^Anonymous:/ { anonymous = $2 }
            END {
                printf "%s,%s,%d,%d,%d,%d,%d,%d,%s\n", renderer, platform,
                    run, seconds, rss, pss, private_clean + private_dirty,
                    anonymous, cpu_percent
            }
        ' "/proc/$pid/smaps_rollup"
    done

    if [[ -n "${NIVALIS_STRESS_STEPS:-}" ]]; then
        stress_result=$(grep 'NIVALIS_STRESS_RESULT' "$log_file" | tail -n 1 || true)
        if [[ -z "$stress_result" ]]; then
            printf 'Stress harness did not report completion before the last sample\n' >&2
            cat "$log_file" >&2
            exit 1
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
