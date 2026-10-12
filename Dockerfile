# syntax=docker/dockerfile:1

# ---- Build: compile the Trunk WASM bundle ----
FROM rust:1-bookworm AS build
ARG APP=wordcraft-web

RUN rustup target add wasm32-unknown-unknown \
 && cargo install --locked trunk

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