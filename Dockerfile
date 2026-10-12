# --- Compilation ---
FROM rust:1.99-slim-trixie AS build
# cmake : compilation de la bibliothèque opus
RUN apt-get update && apt-get install -y --no-install-recommends cmake make g++ \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY assets assets
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release && cp target/release/ratus /ratus

# --- Exécution ---
FROM debian:trixie-slim
ARG YTDLP_VERSION=2026.08.19
ARG DENO_VERSION=2.9.7
# yt-dlp autonome (Python inclus) + Deno (défis JavaScript de YouTube)
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl unzip \
 && mkdir /opt/yt-dlp && curl -fsSL -o /opt/yt-dlp/yt-dlp https://github.com/yt-dlp/yt-dlp/releases/download/${YTDLP_VERSION}/yt-dlp_linux \
 && curl -fsSL -o /tmp/deno.zip https://github.com/denoland/deno/releases/download/v${DENO_VERSION}/deno-x86_64-unknown-linux-gnu.zip \
 && unzip /tmp/deno.zip -d /usr/local/bin && rm /tmp/deno.zip \
 && apt-get purge -y curl unzip && apt-get autoremove -y && rm -rf /var/lib/apt/lists/* \
 && chmod +x /opt/yt-dlp/yt-dlp \
 && useradd --system --no-create-home ratus && chown -R ratus /opt/yt-dlp
COPY --from=build /ratus /usr/local/bin/ratus
USER ratus
# /opt/yt-dlp appartient à ratus : yt-dlp -U peut remplacer son propre binaire
ENV PATH=/opt/yt-dlp:$PATH \
    TOKIO_WORKER_THREADS=2 \
    PIPER_URL=http://piper:5000
# yt-dlp casse quand YouTube change : mise à jour à chaque démarrage (un redémarrage du conteneur suffit à réparer)
CMD ["sh", "-c", "yt-dlp -U >/dev/null 2>&1; exec ratus"]
