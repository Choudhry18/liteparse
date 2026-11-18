#!/bin/bash
# Usage: timeit.sh <binary> <pdf> [runs]  -> median wall time in ms over runs (default 7)
bin=$1; pdf=$2; runs=${3:-7}
for i in $(seq $runs); do
  s=$(python3 -c 'import time;print(time.perf_counter_ns())')
  "$bin" parse --no-ocr -q --format markdown "$pdf" >/dev/null
  e=$(python3 -c 'import time;print(time.perf_counter_ns())')
  echo $(( (e - s) / 1000000 ))
done | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'
