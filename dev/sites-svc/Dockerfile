FROM golang:1.25-alpine AS build
WORKDIR /src
COPY dev/sites-svc/go.mod dev/sites-svc/go.sum ./
RUN go mod download
COPY dev/sites-svc/ ./
RUN CGO_ENABLED=0 GOOS=linux GOARCH=$(go env GOARCH) \
    go build -trimpath -ldflags "-s -w" -o /out/sites-svc .

FROM alpine:3.20
RUN apk add --no-cache curl ca-certificates && \
    addgroup -S app && adduser -S -G app app
COPY --from=build /out/sites-svc /usr/local/bin/sites-svc
EXPOSE 8081
HEALTHCHECK --interval=10s --timeout=3s --retries=10 \
  CMD curl -fsS http://127.0.0.1:8081/livez || exit 1
USER app
ENTRYPOINT ["sites-svc"]
