#!/bin/sh
# Checks the decision's structure. It cannot check whether the claims are true.
set -eu
answer="$HIVEQUEEN_INPUT_DECIDE_ANSWER"
for heading in "## Conclusion" "## Disagreements" "## Unsupported claims"; do
  grep -qF "$heading" "$answer" || { echo "missing heading: $heading" >&2; exit 1; }
done
echo "decision has all required sections"
