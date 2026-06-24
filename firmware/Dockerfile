# Reproducible build image for the CN28 prober firmware (evc04#66), pinned to the
# espup toolchain we target (Xtensa Rust 1.95.0.0). Build from the REPO ROOT so
# the sibling /core crate is in the build context:
#
#   docker build -f firmware/Dockerfile -t evc04-cn28-prober .
#
# For fast local iteration use firmware/docker-build.sh instead (mounts the tree).
#
# WiFi/MQTT creds are compiled into the binary (env!). The defaults are
# placeholders for a CI compile check; pass real values with --build-arg for a
# flashable artifact and treat the resulting image as a secret.
FROM espressif/idf-rust:esp32_1.95.0.0

ARG WIFI_SSID=placeholder
ARG WIFI_PASSWORD=placeholder
ARG MQTT_URL=mqtt://localhost:1883
ENV WIFI_SSID=$WIFI_SSID \
    WIFI_PASSWORD=$WIFI_PASSWORD \
    MQTT_URL=$MQTT_URL

COPY --chown=esp:esp core /project/core
COPY --chown=esp:esp firmware /project/firmware
WORKDIR /project/firmware

RUN cargo build --release
