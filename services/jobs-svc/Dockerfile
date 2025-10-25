FROM golang:1.25-alpine AS build
WORKDIR /src
COPY services/jobs-svc/go.mod services/jobs-svc/go.sum ./
RUN go mod download
COPY services/jobs-svc/ ./
RUN CGO_ENABLED=0 GOOS=linux GOARCH=$(go env GOARCH) \
    go build -trimpath -ldflags "-s -w" -o /out/jobs-svc .

FROM alpine:3.20
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
