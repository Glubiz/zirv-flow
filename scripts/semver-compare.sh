#!/usr/bin/env bash
# SemVer 2.0 precedence: prints -1, 0 or 1; invalid input exits 2.
# Also sourced by changelog/release scripts; no arithmetic on version identifiers.
# Sourced comparators set semver_result without printing or spawning subshells.
set -euo pipefail

semver_valid() {
  local version="$1" identifier pre
  local pattern='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'
  [[ "$version" =~ $pattern ]] || return 1
  version=${version%%+*}
  if [[ "$version" == *-* ]]; then
    pre=${version#*-}
    local -a identifiers
    IFS=. read -r -a identifiers <<< "$pre"
    for identifier in "${identifiers[@]}"; do
      [[ "$identifier" =~ ^0[0-9]+$ ]] && return 1
    done
  fi
  return 0
}

semver_identifier_compare() {
  local left="$1" right="$2" LC_ALL=C
  if [[ "$left" == "$right" ]]; then
    semver_result=0
  elif [[ "$left" =~ ^[0-9]+$ && "$right" =~ ^[0-9]+$ ]]; then
    if (( ${#left} < ${#right} )); then semver_result=-1
    elif (( ${#left} > ${#right} )); then semver_result=1
    elif [[ "$left" < "$right" ]]; then semver_result=-1
    else semver_result=1; fi
  elif [[ "$left" =~ ^[0-9]+$ ]]; then semver_result=-1
  elif [[ "$right" =~ ^[0-9]+$ ]]; then semver_result=1
  elif [[ "$left" < "$right" ]]; then semver_result=-1
  else semver_result=1; fi
}

semver_compare() {
  local left="$1" right="$2" i
  if ! semver_valid "$left" || ! semver_valid "$right"; then
    echo "invalid SemVer: $left or $right" >&2
    return 2
  fi
  left=${left%%+*}; right=${right%%+*}
  local -a a b
  IFS=. read -r -a a <<< "${left%%-*}"
  IFS=. read -r -a b <<< "${right%%-*}"
  for i in 0 1 2; do
    semver_identifier_compare "${a[$i]}" "${b[$i]}"
    if [[ "$semver_result" != 0 ]]; then return; fi
  done
  if [[ "$left" == "$right" ]]; then semver_result=0; return; fi
  if [[ "$left" != *-* ]]; then semver_result=1; return; fi
  if [[ "$right" != *-* ]]; then semver_result=-1; return; fi
  IFS=. read -r -a a <<< "${left#*-}"
  IFS=. read -r -a b <<< "${right#*-}"
  for ((i = 0; i < ${#a[@]} && i < ${#b[@]}; i++)); do
    semver_identifier_compare "${a[$i]}" "${b[$i]}"
    if [[ "$semver_result" != 0 ]]; then return; fi
  done
  semver_identifier_compare "${#a[@]}" "${#b[@]}"
}

semver_self_test() {
  local left right expected actual invalid status
  while read -r left right expected; do
    semver_compare "$left" "$right"
    actual=$semver_result
    [[ "$actual" == "$expected" ]] || { echo "FAIL: $left vs $right: $actual != $expected" >&2; return 1; }
    semver_compare "$right" "$left"
    actual=$semver_result
    [[ "$actual" == "$((-expected))" ]] || return 1
  done <<'CASES'
0.142.0 1.0.0-rc.1 -1
1.0.0-rc.1 1.0.0-rc.2 -1
1.0.0-rc.2 1.0.0-rc.9 -1
1.0.0-rc.9 1.0.0-rc.10 -1
1.0.0-rc.10 1.0.0 -1
1.0.0 1.0.0 0
1.0.0+first 1.0.0+second 0
1.0.0-rc.1+001 1.0.0-rc.1+build-x 0
1.0.0-alpha 1.0.0-alpha.1 -1
1.0.0-alpha.1 1.0.0-alpha.beta -1
1.0.0-alpha.beta 1.0.0-beta -1
1.0.0-beta 1.0.0-beta.2 -1
1.0.0-beta.2 1.0.0-beta.11 -1
1.0.0-beta.11 1.0.0-rc.1 -1
1.0.0-99999999999999999999 1.0.0-100000000000000000000 -1
99999999999999999999.0.0 100000000000000000000.0.0 -1
1.9.0 1.10.0 -1
1.0.9 1.0.10 -1
1.0.0-0 1.0.0-0a -1
1.0.0-A 1.0.0-a -1
CASES
  for invalid in '' v1.0.0 1 1.0 01.0.0 1.01.0 1.0.01 1.0.0- 1.0.0-01 1.0.0-rc.01 1.0.0-rc..1 1.0.0+ 1.0.0+a..b 1.0.0+a+b '1.0.0 rc.1'; do
    status=0
    semver_compare "$invalid" 1.0.0 >/dev/null 2>&1 || status=$?
    [[ "$status" == 2 ]] || { echo "FAIL: accepted malformed version '$invalid'" >&2; return 1; }
  done
  echo 'semver-compare self-test OK'
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  if [[ "${1:-}" == --self-test && $# == 1 ]]; then
    semver_self_test
  elif [[ $# == 2 ]]; then
    semver_compare "$1" "$2"
    echo "$semver_result"
  else
    echo "usage: $0 <left> <right> | --self-test" >&2
    exit 2
  fi
fi
