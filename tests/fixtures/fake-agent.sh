#!/bin/sh
# Fake agent CLI for tests. Behaves like droid, claude, or codex depending on
# FAKE_KIND, and never contacts a provider.
# FAKE_REPLY sets the answer text; FAKE_EXIT forces an exit status;
# FAKE_SLEEP delays the reply.
kind="${FAKE_KIND:-droid}"
for arg in "$@"; do
  case "$arg" in
    --version) echo "fake-$kind 1.0.0"; exit 0 ;;
    --help)
      echo "--output-format --file --cwd --auto --model --reasoning-effort"
      echo "--print --permission-mode --allowedTools --effort --max-turns"
      echo "--ask-for-approval --json --sandbox --cd --skip-git-repo-check --config"
      exit 0 ;;
  esac
done
[ -n "$FAKE_SLEEP" ] && sleep "$FAKE_SLEEP"
[ -n "$FAKE_LOG" ] && echo "$kind $*" >> "$FAKE_LOG"
reply="${FAKE_REPLY:-answer from $kind}"
case "$kind" in
  codex)
    cat > /dev/null
    printf '{"type":"thread.started","thread_id":"t-1"}\n{"type":"turn.started"}\n'
    printf '{"type":"item.completed","item":{"id":"i","type":"agent_message","text":"%s"}}\n' "$reply"
    printf '{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":2}}\n' ;;
  claude)
    cat > /dev/null
    printf '{"type":"result","subtype":"success","is_error":false,"result":"%s","session_id":"c-1","modelUsage":{"fake-claude":{}}}\n' "$reply" ;;
  *)
    printf '{"type":"result","subtype":"success","is_error":false,"result":"%s","session_id":"d-1","usage":{"input_tokens":3}}\n' "$reply" ;;
esac
exit "${FAKE_EXIT:-0}"
