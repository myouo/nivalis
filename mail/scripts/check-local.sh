#!/usr/bin/env bash

set -euo pipefail

mode=${1:-}

fail() {
    printf 'Local quality gate: %s\n' "$1" >&2
    exit 1
}

require_index_snapshot() {
    if ! git diff --quiet --ignore-submodules --; then
        fail "stage or restore all tracked changes before committing"
    fi

    if test -n "$(git ls-files --others --exclude-standard)"; then
        fail "stage or ignore all untracked files before committing"
    fi
}

require_clean_head() {
    if test -n "$(git status --porcelain=v1 --untracked-files=normal)"; then
        fail "commit or restore all changes before pushing"
    fi
}

require_pushed_refs_match_head() {
    local head_oid local_ref local_oid remote_ref remote_oid pushed_commit
    local zero_oid=0000000000000000000000000000000000000000
    head_oid=$(git rev-parse --verify HEAD)

    while read -r local_ref local_oid remote_ref remote_oid; do
        test -n "${local_ref:-}" || continue
        test "$local_oid" != "$zero_oid" || continue

        pushed_commit=$(git rev-parse --verify "${local_oid}^{commit}") ||
            fail "pushed ref $local_ref does not resolve to a commit"
        test "$pushed_commit" = "$head_oid" ||
            fail "check out $local_ref and test its commit before pushing"
    done
}

run_commit_checks() {
    cargo fmt --all -- --check
    cargo test --locked
}

case "$mode" in
    commit)
        require_index_snapshot
        run_commit_checks
        ;;
    push)
        require_clean_head
        require_pushed_refs_match_head
        run_commit_checks
        cargo test --locked --features bench-harness
        cargo clippy --locked --all-targets --all-features -- -D warnings
        ;;
    *)
        printf 'Usage: %s commit|push\n' "$0" >&2
        exit 2
        ;;
esac
