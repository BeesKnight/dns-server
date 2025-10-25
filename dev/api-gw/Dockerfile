FROM golang:1.25-alpine AS build
WORKDIR /src
COPY dev/api-gw/go.mod dev/api-gw/go.sum ./
RUN go mod download
COPY dev/api-gw/ ./
RUN CGO_ENABLED=0 GOOS=linux GOARCH=$(go env GOARCH) \
    go build -trimpath -ldflags "-s -w" -o /out/api-gw .

FROM alpine:3.20
RUN apk add --no-cache curl ca-certificates && \
    addgroup -S app && adduser -S -G app app
COPY --from=build /out/api-gw /usr/local/bin/api-gw
EXPOSE 8088
HEALTHCHECK --interval=10s --timeout=3s --retries=10 \
  CMD curl -fsS http://127.0.0.1:8088/livez || exit 1
USER app
ENTRYPOINT ["api-gw"]
