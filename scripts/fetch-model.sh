#!/usr/bin/env sh
# Download the whisper model parlad expects by default.
#
# Usage: scripts/fetch-model.sh [model-name]
#   model-name defaults to ggml-large-v3-turbo. The file lands in
#   $XDG_DATA_HOME/parla/models (or ~/.local/share/parla/models), matching
#   AsrConfig's default model_path. Re-running with the file present is a no-op.
set -eu

MODEL="${1:-ggml-large-v3-turbo}"
case "$MODEL" in
  *.bin) ;;
  *) MODEL="$MODEL.bin" ;;
esac

DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
DIR="$DATA_HOME/parla/models"
DEST="$DIR/$MODEL"
URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main/$MODEL"

if [ -s "$DEST" ]; then
  echo "already present: $DEST"
  exit 0
fi

command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; exit 1; }

mkdir -p "$DIR"
echo "downloading $URL"
echo "        to $DEST"
# Resumable, and only renamed into place once complete so a partial download
# never passes for a model.
curl --fail --location --continue-at - --progress-bar -o "$DEST.part" "$URL"
mv "$DEST.part" "$DEST"
echo "done: $DEST"
