#!/usr/bin/env bash
# bench-mode.sh — Apply system tuning for maximum benchmark performance
# Usage: sudo ./scripts/bench-mode.sh [on|off]
set -euo pipefail

MODE="${1:-on}"

if [[ "$MODE" == "on" ]]; then
    echo "=== Enabling benchmark mode ==="

    # TCP tuning
    sysctl -w net.ipv4.tcp_fin_timeout=15
    sysctl -w net.core.somaxconn=8192
    sysctl -w net.ipv4.tcp_max_tw_buckets=20000
    sysctl -w net.core.netdev_max_backlog=8192
    sysctl -w net.ipv4.tcp_max_syn_backlog=8192
    sysctl -w net.ipv4.tcp_tw_reuse=1

    # Memory overcommit (Valkey recommends this)
    sysctl -w vm.overcommit_memory=1

    # Disable transparent hugepages (reduces latency jitter)
    if [[ -f /sys/kernel/mm/transparent_hugepage/enabled ]]; then
        echo never > /sys/kernel/mm/transparent_hugepage/enabled
        echo never > /sys/kernel/mm/transparent_hugepage/defrag
        echo "  THP disabled"
    fi

    # Raise file descriptor limits for current shell
    ulimit -n 524288 2>/dev/null || true

    echo "=== Benchmark mode ON ==="

elif [[ "$MODE" == "off" ]]; then
    echo "=== Restoring default settings ==="

    sysctl -w net.ipv4.tcp_fin_timeout=60
    sysctl -w net.core.somaxconn=4096
    sysctl -w net.ipv4.tcp_max_tw_buckets=16384
    sysctl -w net.core.netdev_max_backlog=4096
    sysctl -w net.ipv4.tcp_max_syn_backlog=4096

    sysctl -w vm.overcommit_memory=0

    if [[ -f /sys/kernel/mm/transparent_hugepage/enabled ]]; then
        echo madvise > /sys/kernel/mm/transparent_hugepage/enabled
        echo madvise > /sys/kernel/mm/transparent_hugepage/defrag
        echo "  THP restored to madvise"
    fi

    echo "=== Benchmark mode OFF ==="
else
    echo "Usage: sudo $0 [on|off]"
    exit 1
fi
