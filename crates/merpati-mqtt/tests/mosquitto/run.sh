#!/bin/sh
# Starts an eclipse-mosquitto 2 container for tests/mosquitto.rs:
#
#   crates/merpati-mqtt/tests/mosquitto/run.sh <work-dir>
#   MERPATI_MOSQUITTO=127.0.0.1 cargo test -p merpati-mqtt --test mosquitto -- --ignored
#   docker rm -f merpati-mosquitto
#
# Host ports: 18830 mqtt, 19001 ws, 18883 mqtts, 19443 wss. A throwaway CA
# and server certificate (localhost, 127.0.0.1) are generated in <work-dir>.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
work="${1:?usage: run.sh <work-dir>}"
certs="$work/certs"
mkdir -p "$certs"
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$certs/ca.key" -out "$certs/ca.pem" -days 30 \
  -subj "/CN=Merpati MQTT Test CA" 2>/dev/null
openssl req -newkey rsa:2048 -nodes -keyout "$certs/server.key" -out "$certs/server.csr" \
  -subj "/CN=localhost" 2>/dev/null
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\n' > "$certs/server.ext"
openssl x509 -req -in "$certs/server.csr" -CA "$certs/ca.pem" -CAkey "$certs/ca.key" -CAcreateserial \
  -out "$certs/server.pem" -days 30 -extfile "$certs/server.ext" 2>/dev/null
# The broker runs as an unprivileged user in the container.
chmod 644 "$certs"/*
docker rm -f merpati-mosquitto >/dev/null 2>&1 || true
docker run -d --name merpati-mosquitto \
  -p 127.0.0.1:18830:1883 -p 127.0.0.1:19001:9001 -p 127.0.0.1:18883:8883 -p 127.0.0.1:19443:8884 \
  -v "$here/mosquitto.conf:/mosquitto/config/mosquitto.conf:ro" -v "$certs:/mosquitto/certs:ro" \
  eclipse-mosquitto:2 >/dev/null
echo "mosquitto running (container merpati-mosquitto)"
