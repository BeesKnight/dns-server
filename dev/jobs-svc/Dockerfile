ARG DOCKER_LIBRARY_REGISTRY=public.ecr.aws/docker/library
FROM ${DOCKER_LIBRARY_REGISTRY}/golang:1.25-alpine AS build
WORKDIR /src
COPY dev/jobs-svc/go.mod dev/jobs-svc/go.sum ./
RUN go mod download
COPY dev/jobs-svc/ ./
RUN CGO_ENABLED=0 GOOS=linux GOARCH=$(go env GOARCH) \
    go build -trimpath -ldflags "-s -w" -o /out/jobs-svc .

FROM ${DOCKER_LIBRARY_REGISTRY}/alpine:3.20
# curl для healthcheck, ca-certificates на всякий случай; /data — том из compose
RUN apk add --no-cache curl ca-certificates && \
    addgroup -S app && adduser -S -G app app && \
    mkdir -p /data && chown app:app /data
COPY --from=build /out/jobs-svc /usr/local/bin/jobs-svc
EXPOSE 8082
HEALTHCHECK --interval=10s --timeout=3s --retries=10 \
  CMD curl -fsS http://127.0.0.1:8082/livez || exit 1
VOLUME ["/data"]
USER app
ENTRYPOINT ["jobs-svc"]
