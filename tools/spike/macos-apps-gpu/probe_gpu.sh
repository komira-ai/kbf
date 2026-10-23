#!/bin/bash
# Usage: probe_gpu.sh <bindir>. Prints who runs it, the Metal probe, the MLX probe.
bin=$1
echo "WHO $(id)"
echo "SESSION $(launchctl managername 2>&1)"
ml=""; [ -f "$bin/probe.metallib" ] && ml="$bin/probe.metallib"
perl -e 'alarm shift; exec @ARGV' 120 "$bin/gpu_probe" $ml 2>&1 | sed 's/^/METAL /'
echo "METAL_EXIT ${PIPESTATUS[0]}"
if [ -x "$bin/venv/bin/python" ]; then
  perl -e 'alarm shift; exec @ARGV' 180 "$bin/venv/bin/python" "$bin/mlx_probe.py" 2>&1 | sed 's/^/MLXOUT /'
  echo "MLX_EXIT ${PIPESTATUS[0]}"
else
  echo "MLX_EXIT none (no venv)"
fi
echo "IOREG $(ioreg -r -c IOAccelerator -d1 2>&1 | head -c 300 | tr '\n' ' ')"
