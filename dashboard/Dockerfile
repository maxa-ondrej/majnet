# syntax=docker/dockerfile:1
# Dashboard image: build the Vite/React app, serve the static bundle via nginx
# (which also proxies /api/bot and /api/recon to the WG-internal APIs).
FROM node:22-slim AS build
WORKDIR /app
# Build metadata (CI-injected) baked into the bundle so the page can tell when
# it's a stale cached build vs the one that's actually deployed (design §16).
ARG VERSION=dev
ARG GIT_COMMIT=unknown
ARG BUILD_TIME=unknown
ENV VITE_BUILD_VERSION=$VERSION \
    VITE_BUILD_COMMIT=$GIT_COMMIT \
    VITE_BUILD_TIME=$BUILD_TIME
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

FROM nginx:alpine
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=build /app/dist /usr/share/nginx/html
