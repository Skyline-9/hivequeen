#!/bin/sh
# Fails unless both counts are positive whole numbers.
set -eu
lines=$(tr -d ' \n' < "$HIVEQUEEN_INPUT_LINES_ANSWER")
words=$(tr -d ' \n' < "$HIVEQUEEN_INPUT_WORDS_ANSWER")
[ "$lines" -gt 0 ] && [ "$words" -gt 0 ]
echo "notes.md has $lines lines and $words words"
