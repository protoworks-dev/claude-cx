#!/bin/sh
# Check commit subjects against Conventional Commits 1.0.0.
#
#   scripts/check-commits.sh <file>         one message in a file (the commit-msg hook)
#   scripts/check-commits.sh --range A..B   every commit in a range (CI)
#   scripts/check-commits.sh --title TEXT   one subject line (a pull request title)
#
# A subject is `<type>[(scope)][!]: <description>`: a type from the list below, lower case, a
# description that starts lower case and has no full stop at the end, at most 72 characters.
# A breaking change carries `!` before the colon or a `BREAKING CHANGE:` footer.
# Merge and revert commits git writes itself pass as they are.
#
# Turn the hook on in a clone with:  git config core.hooksPath .githooks
set -eu
types='feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert'

check() { # check <subject> <where>
  subject=$1
  case "$subject" in
    "Merge "* | "Revert \""* | "fixup! "* | "squash! "*) return 0 ;;
  esac
  if ! printf '%s\n' "$subject" | grep -Eq "^($types)(\\([a-z0-9][a-z0-9./-]*\\))?!?: [^A-Z ].*[^. ]\$"; then
    echo "$2: not a Conventional Commit: $subject" >&2
    echo "  want: <type>[(scope)][!]: <description>   types: $(echo "$types" | tr '|' ' ')" >&2
    echo "  the description starts lower case and does not end with a full stop" >&2
    return 1
  fi
  if [ "${#subject}" -gt 72 ]; then
    echo "$2: the subject is ${#subject} characters; keep it to 72: $subject" >&2
    return 1
  fi
}

case "${1:-}" in
  --range)
    failed=0
    for commit in $(git rev-list --no-merges "$2"); do
      check "$(git log -1 --format=%s "$commit")" "$(git rev-parse --short "$commit")" || failed=1
    done
    exit "$failed"
    ;;
  --title) check "$2" "title" ;;
  "") echo "usage: check-commits.sh <file> | --range A..B | --title TEXT" >&2; exit 2 ;;
  *) check "$(sed -e '/^#/d' "$1" | sed -n '/./{p;q;}')" "commit message" ;;
esac
