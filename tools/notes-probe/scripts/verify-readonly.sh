#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
runner="$script_dir/../notes-probe"

run_probe() {
  label=$1
  shift
  result=$("$runner" "$@")
  printf '%s\n' "$result" | /usr/bin/ruby -rjson -e 'JSON.parse(STDIN.read)'
  printf '%s: valid JSON\n' "$label"
}

run_probe accounts accounts
run_probe folders folders
run_probe notes notes --limit 5
run_probe snapshot snapshot --limit 5
