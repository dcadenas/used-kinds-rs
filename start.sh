#!/usr/bin/env bash
# Supervise qdrant + the app in one container (Render web services have no
# sidecars). Exits when either process dies so the platform restarts the pair.
set -u

# exec so $! is qdrant itself, not a wrapper subshell
(cd /qdrant && exec ./qdrant) &
qdrant_pid=$!

app_pid=""
shutdown() {
    kill -TERM ${app_pid:-} ${qdrant_pid:-} 2>/dev/null
}
trap shutdown TERM INT

# Wait for qdrant to accept gRPC connections before starting the app;
# the app hard-fails at startup if qdrant is unreachable.
for _ in $(seq 1 60); do
    if (exec 3<>/dev/tcp/127.0.0.1/6334) 2>/dev/null; then
        break
    fi
    if ! kill -0 "$qdrant_pid" 2>/dev/null; then
        echo "qdrant exited before becoming ready" >&2
        exit 1
    fi
    sleep 1
done

/used-kinds-rs &
app_pid=$!

wait -n
code=$?
shutdown
exit $code
