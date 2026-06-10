#!/usr/bin/env bash
# Supervise qdrant + the app in one container (Render web services have no
# sidecars). Exits when either process dies so the platform restarts the pair.
# When QDRANT_URL points at another host (docker compose dev), there is no
# qdrant to launch or supervise: wait for that one, then become the app.
set -u

# How long to wait for qdrant to accept connections. The old hard-coded 60s
# was not enough for a WAL replay on a slow disk after an unclean stop; a
# too-short cap turns one slow boot into a restart loop that never recovers.
ready_timeout="${QDRANT_READY_TIMEOUT_SECS:-300}"

# Parse host and port out of QDRANT_URL; scheme and path are optional.
qdrant_url="${QDRANT_URL:-http://127.0.0.1:6334}"
hostport="${qdrant_url#*://}"
hostport="${hostport%%/*}"
qdrant_host="${hostport%%:*}"
qdrant_port="${hostport##*:}"
if [ "$qdrant_port" = "$qdrant_host" ]; then
    qdrant_port=6334
fi

if [ "$qdrant_host" != "127.0.0.1" ] && [ "$qdrant_host" != "localhost" ]; then
    echo "Waiting for external qdrant at $qdrant_host:$qdrant_port (timeout ${ready_timeout}s)"
    for _ in $(seq 1 "$ready_timeout"); do
        if (exec 3<>"/dev/tcp/$qdrant_host/$qdrant_port") 2>/dev/null; then
            exec /used-kinds-rs
        fi
        sleep 1
    done
    echo "qdrant at $qdrant_host:$qdrant_port not ready after ${ready_timeout}s, giving up" >&2
    exit 1
fi

# exec so $! is qdrant itself, not a wrapper subshell
(cd /qdrant && exec ./qdrant) &
qdrant_pid=$!

app_pid=""
shutdown() {
    kill -TERM ${app_pid:-} ${qdrant_pid:-} 2>/dev/null
}
trap shutdown TERM INT

# Signal both processes, reap them, then exit. The reap matters: PID 1
# exiting tears down the container's PID namespace and SIGKILLs anything
# still flushing (qdrant). A trapped signal interrupts `wait` (>128), so
# retry until every child is gone.
die() {
    code=$1
    shift
    if [ $# -gt 0 ]; then echo "$*" >&2; fi
    shutdown
    until wait; do :; done
    exit "$code"
}

# Wait for qdrant to accept gRPC connections before starting the app;
# the app hard-fails at startup if qdrant is unreachable.
ready=""
for _ in $(seq 1 "$ready_timeout"); do
    if (exec 3<>/dev/tcp/127.0.0.1/6334) 2>/dev/null; then
        ready=1
        break
    fi
    if ! kill -0 "$qdrant_pid" 2>/dev/null; then
        die 1 "qdrant exited before becoming ready"
    fi
    sleep 1
done
if [ -z "$ready" ]; then
    die 1 "qdrant not ready after ${ready_timeout}s, giving up"
fi

/used-kinds-rs &
app_pid=$!

wait -n
die $?
