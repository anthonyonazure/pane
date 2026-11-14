#!/bin/sh
# Puts this project on GitHub as a PUBLIC repository under your account and uploads it.
# Run from this folder:  sh publish-to-github.sh
# (Needs the GitHub tool, signed in:  brew install gh && gh auth login)
set -e
cd "$(dirname "$0")"
gh repo create pane --public --source . --remote origin --push \
  --description "Use your Android phone or iPhone from your Mac"
echo "Done. Open it with:  gh repo view pane --web"
