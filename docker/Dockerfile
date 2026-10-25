# Merpati CLI image (FR-L3).
#   docker build -f docker/Dockerfile -t merpati .
#   docker run --rm -v "$PWD:/work" merpati run collection.json -e env.json -r cli,junit
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --profile dist --locked -p merpati-cli --bin merpati \
    && strip target/dist/merpati

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 1000 merpati
COPY --from=build /src/target/dist/merpati /usr/local/bin/merpati
USER merpati
WORKDIR /work
# No keychain in containers: workspace secrets come from .merpati/secrets.json.
ENV MERPATI_NO_KEYCHAIN=1
ENTRYPOINT ["merpati"]
CMD ["--help"]
