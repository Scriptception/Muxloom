#!/usr/bin/env bash
set -euo pipefail

printf 'Fake agent is working…\n'
printf '{"hook_event_name":"Notification","notification_type":"idle_prompt","message":"Fake agent needs a decision"}\n' |
  muxloom hook --provider fake --state input
printf 'Prompt> '
IFS= read -r reply
printf 'Received: %s\n' "$reply"
printf '{"hook_event_name":"Stop","message":"Fake agent completed"}\n' |
  muxloom hook --provider fake --state completed
