#!/usr/bin/env bash
# File-based message bus for the multi-angle code-review crew.
# Every reviewer skill and the aggregator talk to each other only through here.
# See PROTOCOL.md for the contract.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUNS="$ROOT/runs"
DEFAULT_ROSTER="correctness,security,tenancy,performance,frontend,tests,conventions"

die(){ printf 'bus: %s\n' "$*" >&2; exit 1; }
now(){ date -u +%Y-%m-%dT%H:%M:%SZ; }
stamp(){ date -u +%Y%m%dT%H%M%S; }
rd(){ printf '%s/%s' "$RUNS" "$1"; }
need(){ [ -d "$(rd "$1")" ] || die "unknown run '$1' (try: bus.sh ls)"; }
rank(){ printf '{"critical":0,"high":1,"medium":2,"low":3}'; }

cmd=${1:-help}; shift || true

case "$cmd" in

init) # init <run-id> --target <text> [--roster a,b,c] [--effort low|medium|high] [--base <git-ref>]
  run=${1:?usage: init <run-id> --target <text>}; shift
  target=""; roster="$DEFAULT_ROSTER"; effort="medium"; base=""
  while [ $# -gt 0 ]; do case "$1" in
    --target) target=${2:-}; shift 2;;
    --roster) roster=${2:-}; shift 2;;
    --effort) effort=${2:-}; shift 2;;
    --base)   base=${2:-};   shift 2;;
    *) die "init: unknown flag '$1'";;
  esac; done
  [ -n "$target" ] || die "init: --target is required"
  d=$(rd "$run"); [ -e "$d" ] && die "run '$run' already exists"
  mkdir -p "$d"/tasks "$d"/findings "$d"/status "$d"/mail
  jq -n --arg id "$run" --arg t "$target" --arg e "$effort" --arg b "$base" \
        --arg ts "$(now)" --arg r "$roster" \
    '{run:$id,target:$t,effort:$e,base:$b,created:$ts,state:"open",
      roster:($r|split(",")|map(select(length>0)))}' > "$d/run.json"
  for r in $(jq -r '.roster[],"aggregator"' "$d/run.json"); do mkdir -p "$d/mail/$r/_read"; done
  echo "$d"
  ;;

ls) # ls -- list known runs, newest first
  [ -d "$RUNS" ] || exit 0
  for f in "$RUNS"/*/run.json; do [ -e "$f" ] || continue
    jq -r '[.run,.state,.created,.target]|@tsv' "$f"; done | sort -k3 -r
  ;;

roster) need "${1:?usage: roster <run-id>}"; jq -r '.roster[]' "$(rd "$1")/run.json" ;;

run) need "${1:?usage: run <run-id>}"; cat "$(rd "$1")/run.json" ;;

assign) # assign <run-id> <reviewer> --focus <text> [--paths a,b] [--notes <text>]
  run=${1:?usage: assign <run-id> <reviewer> --focus <text>}; r=${2:?reviewer}; shift 2; need "$run"
  focus=""; paths=""; notes=""
  while [ $# -gt 0 ]; do case "$1" in
    --focus) focus=${2:-}; shift 2;;
    --paths) paths=${2:-}; shift 2;;
    --notes) notes=${2:-}; shift 2;;
    *) die "assign: unknown flag '$1'";;
  esac; done
  [ -n "$focus" ] || die "assign: --focus is required"
  d=$(rd "$run")
  jq -e --arg r "$r" '.roster|index($r)' "$d/run.json" >/dev/null || die "'$r' is not on this run's roster"
  jq -n --arg run "$run" --arg r "$r" --arg f "$focus" --arg p "$paths" --arg n "$notes" --arg ts "$(now)" \
    '{run:$run,reviewer:$r,assigned:$ts,focus:$f,notes:$n,
      paths:($p|split(",")|map(select(length>0)))}' > "$d/tasks/$r.json"
  jq -n --arg r "$r" --arg ts "$(now)" '{reviewer:$r,state:"assigned",note:"",updated:$ts}' > "$d/status/$r.json"
  : > "$d/findings/$r.jsonl"
  mkdir -p "$d/mail/$r/_read"
  echo "$d/tasks/$r.json"
  ;;

task) # task <run-id> <reviewer> -- the assignment plus shared run context
  need "${1:?usage: task <run-id> <reviewer>}"; d=$(rd "$1"); r=${2:?reviewer}
  [ -f "$d/tasks/$r.json" ] || die "no task for '$r' on run '$1'"
  jq -s '{run:.[0],task:.[1]}' "$d/run.json" "$d/tasks/$r.json"
  ;;

status) # status <run-id> <reviewer> <assigned|working|blocked|done> [note]
  need "${1:?usage: status <run-id> <reviewer> <state> [note]}"; d=$(rd "$1"); r=${2:?reviewer}; s=${3:?state}; n=${4:-}
  case "$s" in assigned|working|blocked|done) ;; *) die "status: bad state '$s'";; esac
  jq -n --arg r "$r" --arg s "$s" --arg n "$n" --arg ts "$(now)" \
    '{reviewer:$r,state:$s,note:$n,updated:$ts}' > "$d/status/$r.json"
  ;;

board) # board <run-id> -- one line per reviewer: state, findings, unread mail
  need "${1:?usage: board <run-id>}"; d=$(rd "$1")
  printf 'run %s  state=%s  target=%s\n' "$1" "$(jq -r .state "$d/run.json")" "$(jq -r .target "$d/run.json")"
  printf '%-14s %-9s %8s %6s  %s\n' REVIEWER STATE FINDINGS MAIL NOTE
  for r in $(jq -r '.roster[]' "$d/run.json"); do
    st=none; note=""
    if [ -f "$d/status/$r.json" ]; then st=$(jq -r .state "$d/status/$r.json"); note=$(jq -r .note "$d/status/$r.json"); fi
    n=0; [ -s "$d/findings/$r.jsonl" ] && n=$(grep -c . "$d/findings/$r.jsonl" || true)
    m=$(find "$d/mail/$r" -maxdepth 1 -name '*.json' 2>/dev/null | wc -l | tr -d ' ')
    printf '%-14s %-9s %8s %6s  %s\n' "$r" "$st" "$n" "$m" "$note"
  done
  ;;

finding) # finding <run-id> <reviewer>   -- one JSON object or array on stdin
  need "${1:?usage: finding <run-id> <reviewer> < findings.json}"; d=$(rd "$1"); r=${2:?reviewer}
  f="$d/findings/$r.jsonl"; touch "$f"
  n=$(grep -c . "$f" || true)
  jq -c 'if type=="array" then .[] else . end' | while IFS= read -r obj; do
    for k in severity title file detail; do
      printf '%s' "$obj" | jq -e --arg k "$k" 'has($k) and (.[$k]|tostring|length>0)' >/dev/null \
        || die "finding is missing required field '$k': $obj"
    done
    sev=$(printf '%s' "$obj" | jq -r .severity)
    case "$sev" in critical|high|medium|low) ;; *) die "bad severity '$sev' (critical|high|medium|low)";; esac
    case "$sev" in critical|high)
      printf '%s' "$obj" | jq -e '(.failure_scenario//""|length)>0' >/dev/null \
        || die "severity '$sev' requires a concrete failure_scenario";;
    esac
    n=$((n+1)); id=$(printf '%s-%03d' "$r" "$n")
    printf '%s' "$obj" | jq -c --arg r "$r" --arg id "$id" --arg ts "$(now)" \
      '{id:(.id // $id),reviewer:$r,reported:$ts,severity,confidence:(.confidence//"likely"),
        category:(.category//"general"),file,line:(.line//null),title,detail,
        failure_scenario:(.failure_scenario//""),fix:(.fix//""),
        evidence:(.evidence//[]),cross_ref:(.cross_ref//[])}' >> "$f"
    echo "$id"
  done
  ;;

collect) # collect <run-id> [reviewer] -- all findings as one severity-sorted JSON array
  need "${1:?usage: collect <run-id> [reviewer]}"; d=$(rd "$1")
  if [ -n "${2:-}" ]; then
    [ -s "$d/findings/$2.jsonl" ] || { echo '[]'; exit 0; }
    set -- "$d/findings/$2.jsonl"
  else
    set --; for g in "$d"/findings/*.jsonl; do [ -s "$g" ] && set -- "$@" "$g"; done; fi
  [ $# -gt 0 ] || { echo '[]'; exit 0; }
  cat "$@" | jq -s --argjson rank "$(rank)" 'sort_by($rank[.severity], .file, (.line//0))'
  ;;

clusters) # clusters <run-id> -- findings grouped by file, to spot cross-reviewer overlap
  need "${1:?usage: clusters <run-id>}"
  "$0" collect "$1" | jq 'group_by(.file)|map({file:.[0].file,count:length,
    reviewers:(map(.reviewer)|unique),
    items:map({id,reviewer,severity,line,title})})|sort_by(-.count)'
  ;;

send) # send <run-id> <from> <to|all> <kind> <subject> [--body <text>|--refs a,b]  (body also from stdin)
  need "${1:?usage: send <run-id> <from> <to> <kind> <subject>}"; run=$1; d=$(rd "$run")
  from=${2:?from}; to=${3:?to}; kind=${4:?kind}; subject=${5:?subject}; shift 5
  body=""; refs=""
  while [ $# -gt 0 ]; do case "$1" in
    --body) body=${2:-}; shift 2;;
    --refs) refs=${2:-}; shift 2;;
    *) die "send: unknown flag '$1'";;
  esac; done
  case "$kind" in question|answer|handoff|corroborate|dispute|fyi|blocker) ;;
    *) die "send: bad kind '$kind' (question|answer|handoff|corroborate|dispute|fyi|blocker)";; esac
  [ -n "$body" ] || { [ -t 0 ] || body=$(cat); }
  [ -n "$body" ] || die "send: empty body (use --body or pipe it in)"
  if [ "$to" = all ]; then targets=$(jq -r --arg me "$from" '.roster[]|select(.!=$me)' "$d/run.json"); else targets=$to; fi
  for t in $targets; do
    [ -d "$d/mail/$t" ] || die "send: no mailbox for '$t'"
    out="$d/mail/$t/$(stamp)-$RANDOM-$from.json"
    jq -n --arg run "$run" --arg f "$from" --arg t "$t" --arg k "$kind" --arg s "$subject" \
          --arg b "$body" --arg r "$refs" --arg ts "$(now)" \
      '{run:$run,ts:$ts,from:$f,to:$t,kind:$k,subject:$s,body:$b,
        refs:($r|split(",")|map(select(length>0)))}' > "$out"
    echo "$out"
  done
  ;;

inbox) # inbox <run-id> <me> [--peek] -- prints unread mail; without --peek marks it read
  need "${1:?usage: inbox <run-id> <me>}"; d=$(rd "$1"); me=${2:?me}; peek=${3:-}
  box="$d/mail/$me"; [ -d "$box" ] || die "no mailbox for '$me'"
  found=0
  for m in $(find "$box" -maxdepth 1 -name '*.json' | sort); do
    found=1; jq -r '"--- [\(.kind)] \(.from) -> \(.to) @ \(.ts)\nsubject: \(.subject)\nrefs: \(.refs|join(", "))\n\(.body)"' "$m"
    [ "$peek" = "--peek" ] || mv "$m" "$box/_read/"
  done
  [ "$found" = 1 ] || echo "(inbox empty)"
  ;;

thread) # thread <run-id> <me> -- everything ever delivered to <me>, read or not
  need "${1:?usage: thread <run-id> <me>}"; d=$(rd "$1"); me=${2:?me}
  find "$d/mail/$me" -name '*.json' | sort | while read -r m; do
    jq -r '"[\(.ts)] [\(.kind)] \(.from): \(.subject)\n\(.body)\n"' "$m"; done
  ;;

close) # close <run-id> [state]
  need "${1:?usage: close <run-id>}"; d=$(rd "$1"); s=${2:-closed}
  tmp=$(mktemp); jq --arg s "$s" --arg ts "$(now)" '.state=$s|.closed=$ts' "$d/run.json" > "$tmp"; mv "$tmp" "$d/run.json"
  ;;

path) need "${1:?usage: path <run-id>}"; rd "$1" ;;

help|*)
  sed -n 's/^\([a-z|]*)\) #/  \1 #/p' "$0" | sed 's/) #/  /'
  printf '\nrun dir: %s/<run-id>\n' "$RUNS"
  ;;
esac
