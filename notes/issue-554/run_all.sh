#!/bin/bash
# Usage: run_all.sh <lit binary> <pdf dir>
for f in "$2"/*.pdf; do
  echo "=================== $(basename "$f")"
  "$1" parse --no-ocr -q --format markdown "$f" | sed '/^$/d'
done
