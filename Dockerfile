# syntax=docker/dockerfile:1

# ---- Build: compile the Trunk WASM bundle ----
FROM rust:1-bookworm AS build
ARG APP=wordcraft-web

# Keep peak memory low so the build is not OOM-killed (exit 137) on small build
# servers: compile one crate at a time and drop debug info.
ENV CARGO_BUILD_JOBS=1 \
    CARGO_INCREMENTAL=0 \
    CARGO_PROFILE_RELEASE_DEBUG=false

# Prebuilt Trunk binary (avoids compiling Trunk from source).
RUN curl -fsSL https://github.com/trunk-rs/trunk/releases/download/v0.21.14/trunk-x86_64-unknown-linux-gnu.tar.gz \
      | tar -xzf - -C /usr/local/bin trunk \
 && rustup target add wasm32-unknown-unknown

WORKDIR /src
COPY . .

# Trunk.toml writes the static site to /src/dist/web (dist = "../../dist/web")
WORKDIR /src/apps/${APP}
RUN trunk build --release

# ---- Serve: static nginx on port 3000 (matches Coolify's default exposed port) ----
FROM nginx:1.27-alpine
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=build /src/dist/web /usr/share/nginx/html
EXPOSE 3000