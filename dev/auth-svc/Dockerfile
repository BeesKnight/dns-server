# --- build stage ---
ARG DOCKER_LIBRARY_REGISTRY=public.ecr.aws/docker/library
FROM ${DOCKER_LIBRARY_REGISTRY}/golang:1.25-alpine AS build
WORKDIR /src
# модули отдельно — быстрее кеш
COPY dev/auth-svc/go.mod dev/auth-svc/go.sum ./
RUN go mod download
# исходники сервиса
COPY dev/auth-svc/ ./
# статический бинарь
RUN CGO_ENABLED=0 GOOS=linux GOARCH=$(go env GOARCH) \
    go build -trimpath -ldflags "-s -w" -o /out/auth-svc .

# --- runtime stage ---
FROM ${DOCKER_LIBRARY_REGISTRY}/alpine:3.20
RUN apk add --no-cache curl ca-certificates && \
    addgroup -S app && adduser -S -G app app
COPY --from=build /out/auth-svc /usr/local/bin/auth-svc
EXPOSE 8080
HEALTHCHECK --interval=10s --timeout=3s --retries=10 \
  CMD curl -fsS http://127.0.0.1:8080/livez || exit 1
USER app
ENTRYPOINT ["auth-svc"]
