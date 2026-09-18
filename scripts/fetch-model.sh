#!/usr/bin/env sh
# Download the models parlad expects by default: the whisper model that
# transcribes speech and the GGUF the local judge reasons with.
#
# Usage: scripts/fetch-model.sh                      both defaults
#        scripts/fetch-model.sh whisper [ggml-name]  e.g. ggml-large-v3-turbo
#        scripts/fetch-model.sh judge [repo file]    a Hugging Face repo and
#                                                    the .gguf inside it
#
# Files land in $XDG_DATA_HOME/parla/models (or ~/.local/share/parla/models),
# matching the config defaults. Re-running with a file present is a no-op.
set -eu

DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
DIR="$DATA_HOME/parla/models"

WHISPER_DEFAULT="ggml-large-v3-turbo"
JUDGE_REPO_DEFAULT="unsloth/Qwen3-4B-Instruct-2507-GGUF"
JUDGE_FILE_DEFAULT="Qwen3-4B-Instruct-2507-Q4_K_M.gguf"

fetch() {
  url="$1"
  dest="$2"
  if [ -s "$dest" ]; then
    echo "already present: $dest"
    return 0
  fi
  command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; exit 1; }
  mkdir -p "$DIR"
  echo "downloading $url"
  echo "        to $dest"
  # Resumable, and only renamed into place once complete so a partial
  # download never passes for a model.
  curl --fail --location --continue-at - --progress-bar -o "$dest.part" "$url"
  mv "$dest.part" "$dest"
  echo "done: $dest"
}

whisper() {
  name="${1:-$WHISPER_DEFAULT}"
  case "$name" in
    *.bin) ;;
    *) name="$name.bin" ;;
  esac
  fetch "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/$name" "$DIR/$name"
}

judge() {
  repo="${1:-$JUDGE_REPO_DEFAULT}"
  file="${2:-$JUDGE_FILE_DEFAULT}"
  fetch "https://huggingface.co/$repo/resolve/main/$file" "$DIR/$file"
}

case "${1:-all}" in
  all) whisper; judge ;;
  whisper) shift; whisper "$@" ;;
  judge) shift; judge "$@" ;;
  *) echo "usage: $0 [whisper [name] | judge [repo file]]" >&2; exit 2 ;;
esac
