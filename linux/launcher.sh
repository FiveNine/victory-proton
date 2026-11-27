#!/bin/bash
# Starts the victory-proton launcher.
exec python3 -I "$(dirname "$(readlink -f "$0")")/launcher/victory_proton.py" "$@"
