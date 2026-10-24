#!/bin/bash
set -euo pipefail

ROOT_DIR="/home/jyash8/Projects/drag_os"
SERIAL_LOG="/tmp/serial.log"
MONITOR_SOCK="/tmp/dragos-mon.sock"
QEMU_PID_FILE="/tmp/qemu.pid"
UEFI_DEBUG_LOG="/tmp/uefi_debug.log"

rm -f "$SERIAL_LOG" "$MONITOR_SOCK" "$QEMU_PID_FILE" "$UEFI_DEBUG_LOG"

# Launch QEMU in the background
( 
  cargo run -p tools -- run --headless --mem 2G --smp 2 --no-audio --trace &> "$SERIAL_LOG" & 
  echo $! > "$QEMU_PID_FILE" 
) &

echo "QEMU launched. PID stored in $QEMU_PID_FILE. Serial output to $SERIAL_LOG"
echo "Waiting for monitor socket $MONITOR_SOCK..."

# Wait for the monitor socket to appear
timeout 60s bash -c "while [ ! -S '$MONITOR_SOCK' ]; do sleep 0.1; done" || {
  echo "Error: QEMU monitor socket did not appear within 60 seconds."
  cat "$SERIAL_LOG"
  exit 1
}

echo "Monitor socket active. QEMU is running headless."

# Allow QEMU to run for a bit
sleep 120 # Run for 2 minutes

# Try to send a quit command to QEMU monitor, if it's still running
if [ -f "$QEMU_PID_FILE" ] && ps -p $(cat "$QEMU_PID_FILE") > /dev/null; then
  echo "Attempting to quit QEMU gracefully..."
  # Use socat to send 'quit' command to QEMU monitor
  # This might fail if the monitor process itself has died or is unresponsive
  echo "quit" | socat - UNIX-CONNECT:"$MONITOR_SOCK" || true
  # Give QEMU a moment to shut down
  sleep 5
fi

# Ensure QEMU process is terminated
if [ -f "$QEMU_PID_FILE" ] && ps -p $(cat "$QEMU_PID_FILE") > /dev/null; then
  echo "QEMU still running, killing process..."
  kill $(cat "$QEMU_PID_FILE") || true
  sleep 2
fi

rm -f "$QEMU_PID_FILE" "$MONITOR_SOCK"
echo "QEMU session ended."

# Extract uefi_debug.log from serial output
grep -A 1000000000 "UEFI DEBUG START" "$SERIAL_LOG" | grep -B 1000000000 "UEFI DEBUG END" > "$UEFI_DEBUG_LOG" || true

echo "Extracted UEFI debug log to $UEFI_DEBUG_LOG"