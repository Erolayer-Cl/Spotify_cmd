#!/usr/bin/env bash
# Pruebas de extremo a extremo de spotify_cmd contra una API de Spotify simulada.
# Requisitos: python3 y `pip install pyte` (emulador de terminal para leer la pantalla).
set -euo pipefail
cd "$(dirname "$0")/../.."
cargo build --release
BIN=./target/release/spotify_cmd
echo "== Funcional (lo que se ve y lo que se envía a la API)"
python3 tests/e2e/functional_test.py "$BIN"
echo "== Caos (la API falla al azar, teclas aleatorias durante 60 s)"
python3 tests/e2e/chaos_test.py "$BIN" 60
echo "== Velocidad (biblioteca de 5000 canciones, 80 ms de latencia)"
python3 tests/e2e/speed_test.py "$BIN"
