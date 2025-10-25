package main

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"database/sql"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/go-chi/chi/v5/middleware"
	"github.com/google/uuid"
	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgconn"
	"github.com/jackc/pgx/v5/pgxpool"
	"github.com/oschwald/geoip2-golang"
	"github.com/redis/go-redis/v9"
	"golang.org/x/crypto/bcrypt"
)

/*************** Config ***************/
type Config struct {
	HTTPAddr      string
	DatabaseURL   string
	RedisAddr     string
	RedisPassword string

	StreamTasks  string
	ClaimBlockMs int64
	LeaseTTL     time.Duration
	MaxRetries   int
	RetryBackoff time.Duration

	DefaultMaxParallel int
	CacheTTL           time.Duration
	QuickDedupTTL      time.Duration
	QuickRateWindow    time.Duration
	QuickRatePerUser   int
	QuickRatePerIP     int

	// SSE/карта
	PubPrefix       string // для check-upd:* каналов (пер-чековые SSE)
	MapPubChannel   string // "map:events"
	MapStream       string // "map_events"
	MapStreamMaxLen int    // MaxLenApprox для истории
	GeoCacheTTL     time.Duration

	// GeoIP
	GeoIPDisabled bool
	GeoIPCityPath string // путь к GeoLite2-City.mmdb
	GeoIPASNPath  string // (опц.) путь к GeoLite2-ASN.mmdb
}

func env(k, def string) string {
	if v := strings.TrimSpace(os.Getenv(k)); v != "" {
		return v
	}
	return def
}
func ienv(k string, def int) int {
	if v := strings.TrimSpace(os.Getenv(k)); v != "" {
		var x int
		if _, err := fmt.Sscan(v, &x); err == nil {
			return x
		}
	}
	return def
}
func denv(k string, def string) time.Duration {
	if v := strings.TrimSpace(os.Getenv(k)); v != "" {
		if d, err := time.ParseDuration(v); err == nil {
			return d
		}
	}
	d, _ := time.ParseDuration(def)
	return d
}
func benv(k string, def bool) bool {
	v := strings.ToLower(strings.TrimSpace(os.Getenv(k)))
	if v == "" {
		return def
	}
	return v == "1" || v == "true" || v == "yes" || v == "y"
}

func loadConfig() Config {
	return Config{
		HTTPAddr:      env("HTTP_ADDR", ":8082"),
		DatabaseURL:   env("DATABASE_URL", "postgres://postgres:dev@postgres:5432/aezacheck?sslmode=disable"),
		RedisAddr:     env("REDIS_ADDR", "redis:6379"),
		RedisPassword: env("REDIS_PASSWORD", ""),

		StreamTasks:  env("STREAM_TASKS", "check_tasks"),
		ClaimBlockMs: int64(ienv("CLAIM_BLOCK_MS", 20000)),
		LeaseTTL:     denv("LEASE_TTL", "90s"),
		MaxRetries:   ienv("MAX_RETRIES", 2),
		RetryBackoff: denv("RETRY_BACKOFF", "5s"),

		DefaultMaxParallel: ienv("DEFAULT_MAX_PARALLEL", 4),
		CacheTTL:           denv("CACHE_TTL", "30s"),
		QuickDedupTTL:      denv("QUICK_DEDUP_TTL", "45s"),
		QuickRateWindow:    denv("QUICK_RATE_WINDOW", "1m"),
		QuickRatePerUser:   ienv("QUICK_RATE_PER_USER", 5),
		QuickRatePerIP:     ienv("QUICK_RATE_PER_IP", 15),

		PubPrefix:       env("PUB_PREFIX", "check-upd:"),
		MapPubChannel:   env("MAP_PUB_CHANNEL", "map:events"),
		MapStream:       env("MAP_STREAM", "map_events"),
		MapStreamMaxLen: ienv("MAP_STREAM_MAXLEN", 100000),
		GeoCacheTTL:     denv("GEO_CACHE_TTL", "2m"),

		GeoIPDisabled: benv("GEOIP_DISABLED", false),
		GeoIPCityPath: env("GEOIP_CITY_PATH", "/opt/aezacheck/data/GeoLite2-City.mmdb"),
		GeoIPASNPath:  env("GEOIP_ASN_PATH", "/opt/aezacheck/data/GeoLite2-ASN.mmdb"),
	}
}

type pgxPool interface {
	Close()
	Ping(context.Context) error
	Query(context.Context, string, ...any) (pgx.Rows, error)
	QueryRow(context.Context, string, ...any) pgx.Row
	Exec(context.Context, string, ...any) (pgconn.CommandTag, error)
}

/*************** App ***************/
type App struct {
	cfg   Config
	log   *slog.Logger
	pool  pgxPool
	redis *redis.Client

	geo      *geoIP    // может быть nil (если выключено/файл не найден)
	geoCache *geoCache // кэш IP→Geo для REST-эндпоинтов
}

func main() {
	cfg := loadConfig()
	logger := slog.New(slog.NewTextHandler(os.Stdout, &slog.HandlerOptions{Level: slog.LevelInfo}))
	slog.SetDefault(logger)

	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()

	app, err := initApp(ctx, cfg, logger)
	if err != nil {
		logger.Error("init", "err", err)
		os.Exit(1)
	}
	defer app.Close()

	// фон: перераздача истёкших аренд
	go app.retryLoop()

	r := chi.NewRouter()
	r.Use(middleware.RequestID, middleware.RealIP, middleware.Recoverer, middleware.Logger, withTimeout(25*time.Second))

	// health
	r.Get("/livez", func(w http.ResponseWriter, _ *http.Request) { w.WriteHeader(http.StatusOK) })
	r.Get("/readyz", func(w http.ResponseWriter, r *http.Request) {
		if err := app.pool.Ping(r.Context()); err != nil {
			http.Error(w, "db not ready", http.StatusServiceUnavailable)
			return
		}
		if err := app.redis.Ping(r.Context()).Err(); err != nil {
			http.Error(w, "redis not ready", http.StatusServiceUnavailable)
			return
		}
		w.WriteHeader(http.StatusOK)
	})

	// агенты
	r.Route("/v1/agents", func(rt chi.Router) {
		rt.Post("/register", app.handleAgentRegister)
		rt.With(app.requireAgent).Post("/heartbeat", app.handleHeartbeat)
		rt.With(app.requireAgent).Post("/claim", app.handleClaim)
		rt.With(app.requireAgent).Post("/extend", app.handleExtend)
		rt.With(app.requireAgent).Post("/report", app.handleReport)
	})

	// карта
	r.Route("/v1", func(rt chi.Router) {
		rt.Get("/map/agents", app.handleMapAgents)
		rt.Get("/map/events", app.handleMapEvents)
		rt.Get("/map/stream", app.handleMapStream)     // SSE
		rt.Get("/map/snapshot", app.handleMapSnapshot) // история из Stream
		rt.Get("/checks/{id}/geo", app.handleCheckGeo)
		rt.Get("/geo/lookup", app.handleGeoLookup)
		rt.Post("/jobs/checks", app.handleQuickCheckCreate)
	})

	srv := &http.Server{Addr: cfg.HTTPAddr, Handler: r, ReadHeaderTimeout: 5 * time.Second}
	go func() {
		logger.Info("http_listen", "addr", cfg.HTTPAddr)
		if err := srv.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
			logger.Error("http", "err", err)
			cancel()
		}
	}()

	<-ctx.Done()
	shut, c2 := context.WithTimeout(context.Background(), 5*time.Second)
	defer c2()
	_ = srv.Shutdown(shut)
}

func initApp(ctx context.Context, cfg Config, log *slog.Logger) (*App, error) {
	pool, err := pgxpool.New(ctx, cfg.DatabaseURL)
	if err != nil {
		return nil, err
	}
	if err := pool.Ping(ctx); err != nil {
		return nil, err
	}
	if err := migrate(ctx, pool); err != nil {
		return nil, err
	}
	rdb := redis.NewClient(&redis.Options{Addr: cfg.RedisAddr, Password: cfg.RedisPassword})
	if err := rdb.Ping(ctx).Err(); err != nil {
		return nil, err
	}

	var gip *geoIP
	if !cfg.GeoIPDisabled {
		if geo, gerr := openGeo(cfg.GeoIPCityPath, cfg.GeoIPASNPath); gerr == nil {
			log.Info("geoip_loaded", "city", cfg.GeoIPCityPath, "asn", cfg.GeoIPASNPath)
			gip = geo
		} else {
			log.Warn("geoip_disabled", "err", gerr)
		}
	}

	return &App{cfg: cfg, log: log, pool: pool, redis: rdb, geo: gip, geoCache: newGeoCache(cfg.GeoCacheTTL)}, nil
}

func (a *App) Close() {
	if a.pool != nil {
		a.pool.Close()
	}
	if a.redis != nil {
		_ = a.redis.Close()
	}
	if a.geo != nil {
		a.geo.Close()
	}
}

/*************** DB migrate ***************/
/*************** Helpers ***************/
func withTimeout(d time.Duration) func(http.Handler) http.Handler {
	return func(next http.Handler) http.Handler { return http.TimeoutHandler(next, d, `{"error":"timeout"}`) }
}
func sha256Hex(s string) string {
	h := sha256.Sum256([]byte(s))
	return hex.EncodeToString(h[:])
}
func parseIP(r *http.Request) string {
	if xff := strings.TrimSpace(r.Header.Get("X-Forwarded-For")); xff != "" {
		parts := strings.Split(xff, ",")
		return strings.TrimSpace(parts[0])
	}
	if xr := strings.TrimSpace(r.Header.Get("X-Real-IP")); xr != "" {
		return xr
	}
	host, _, _ := net.SplitHostPort(r.RemoteAddr)
	return host
}

/*************** Agent auth ***************/
type agentAuth struct {
	ID         uuid.UUID
	ExternalID uint64
	Name       string
	IP         net.IP
	Limit      int
}
type agentAuthKey struct{}

func (a *App) requireAgent(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		idStr := strings.TrimSpace(r.Header.Get("X-Agent-Id"))
		tok := strings.TrimSpace(r.Header.Get("X-Agent-Token"))
		if idStr == "" || tok == "" {
			http.Error(w, "missing agent auth", http.StatusUnauthorized)
			return
		}
		var (
			aid        uuid.UUID
			name       string
			tokenHash  string
			limit      int
			externalID int64
			err        error
		)
		if parsed, perr := uuid.Parse(idStr); perr == nil {
			err = a.pool.QueryRow(r.Context(),
				`select id, external_id, name, token_hash, max_parallel from agents where id=$1 and is_active=true`, parsed).
				Scan(&aid, &externalID, &name, &tokenHash, &limit)
		} else {
			num, nerr := strconv.ParseUint(idStr, 10, 64)
			if nerr != nil {
				http.Error(w, "bad agent id", http.StatusUnauthorized)
				return
			}
			err = a.pool.QueryRow(r.Context(),
				`select id, external_id, name, token_hash, max_parallel from agents where external_id=$1 and is_active=true`, num).
				Scan(&aid, &externalID, &name, &tokenHash, &limit)
		}
		if err != nil {
			http.Error(w, "agent not found", http.StatusUnauthorized)
			return
		}
		if bcrypt.CompareHashAndPassword([]byte(tokenHash), []byte(tok)) != nil {
			http.Error(w, "invalid token", http.StatusUnauthorized)
			return
		}
		ip := net.ParseIP(parseIP(r))
		ctx := context.WithValue(r.Context(), agentAuthKey{}, &agentAuth{ID: aid, ExternalID: uint64(externalID), Name: name, IP: ip, Limit: limit})
		next.ServeHTTP(w, r.WithContext(ctx))
	})
}
func agentFromCtx(ctx context.Context) *agentAuth {
	v, _ := ctx.Value(agentAuthKey{}).(*agentAuth)
	return v
}

/*************** DTOs ***************/
type registerReq struct {
	Hostname    string `json:"hostname,omitempty"`
	Location    string `json:"location,omitempty"`
	Version     string `json:"version,omitempty"`
	MaxParallel *int   `json:"max_parallel,omitempty"`
}
type registerResp struct {
	AgentID            uint64 `json:"agent_id"`
	AuthToken          string `json:"auth_token"`
	LeaseDurationMs    int64  `json:"lease_duration_ms"`
	HeartbeatTimeoutMs int64  `json:"heartbeat_timeout_ms"`
}

type heartbeatReq struct {
	AgentID  uint64 `json:"agent_id"`
	Version  string `json:"version,omitempty"`
	Location string `json:"location,omitempty"`
}
type heartbeatResp struct {
	AgentID        uint64 `json:"agent_id"`
	NextDeadlineMs int64  `json:"next_deadline_ms"`
}

type claimReq struct {
	AgentID    uint64         `json:"agent_id"`
	Capacities map[string]int `json:"capacities,omitempty"`
}

type leaseDTO struct {
	LeaseID      uint64          `json:"lease_id"`
	TaskID       uint64          `json:"task_id"`
	Kind         string          `json:"kind"`
	LeaseUntilMs int64           `json:"lease_until_ms"`
	Spec         json.RawMessage `json:"spec"`
}

type claimResp struct {
	Leases []leaseDTO `json:"leases"`
}

type extendReq struct {
	AgentID    uint64   `json:"agent_id"`
	LeaseIDs   []uint64 `json:"lease_ids"`
	ExtendByMs uint64   `json:"extend_by_ms"`
}

type extendOutcome struct {
	LeaseID       uint64 `json:"lease_id"`
	NewDeadlineMs int64  `json:"new_deadline_ms"`
}

type extendResp struct {
	Outcomes []extendOutcome `json:"outcomes"`
}

type reportReq struct {
	AgentID   uint64        `json:"agent_id"`
	Completed []leaseReport `json:"completed"`
	Cancelled []leaseReport `json:"cancelled"`
}
type leaseReport struct {
	LeaseID      uint64              `json:"lease_id"`
	Observations []reportObservation `json:"observations,omitempty"`
}
type reportObservation struct {
	Name  string          `json:"name"`
	Value json.RawMessage `json:"value"`
	Unit  *string         `json:"unit,omitempty"`
}

type reportResp struct {
	Acknowledged int `json:"acknowledged"`
}

/*************** Handlers: register/heartbeat ***************/
func (a *App) handleAgentRegister(w http.ResponseWriter, r *http.Request) {
	var req registerReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "bad json", http.StatusBadRequest)
		return
	}
	name := strings.TrimSpace(req.Hostname)
	if name == "" {
		name = fmt.Sprintf("agent-%s", strings.ReplaceAll(uuid.NewString()[:8], "-", ""))
	}
	plain, err := newRandomToken(32)
	if err != nil {
		http.Error(w, "token error", http.StatusInternalServerError)
		return
	}
	hash, _ := bcrypt.GenerateFromPassword([]byte(plain), bcrypt.DefaultCost)
	limit := a.cfg.DefaultMaxParallel
	if req.MaxParallel != nil && *req.MaxParallel > 0 {
		limit = *req.MaxParallel
	}
	var (
		id         uuid.UUID
		externalID int64
	)
	agentIP := parseIP(r)
	err = a.pool.QueryRow(r.Context(),
		`insert into agents(name, token_hash, location, version, agent_ip, max_parallel, last_seen)
                 values($1,$2,$3,$4,$5,$6,now()) returning id, external_id`,
		name, string(hash), req.Location, req.Version, agentIP, limit).Scan(&id, &externalID)
	if err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}
	leaseDurationMs := a.cfg.LeaseTTL.Milliseconds()
	if leaseDurationMs <= 0 {
		leaseDurationMs = int64((30 * time.Second).Milliseconds())
	}
	heartbeatTimeout := a.heartbeatTimeout()
	resp := registerResp{
		AgentID:            uint64(externalID),
		AuthToken:          plain,
		LeaseDurationMs:    leaseDurationMs,
		HeartbeatTimeoutMs: heartbeatTimeout.Milliseconds(),
	}
	_ = json.NewEncoder(w).Encode(resp)

	a.publishAgentOnline(r.Context(), id, uint64(externalID), name, agentIP, req.Location, req.Version)
	a.markAgentOnlineSent(r.Context(), id)
}

func (a *App) handleHeartbeat(w http.ResponseWriter, r *http.Request) {
	ag := agentFromCtx(r.Context())
	if ag == nil {
		http.Error(w, "unauthorized", http.StatusUnauthorized)
		return
	}
	var req heartbeatReq
	_ = json.NewDecoder(r.Body).Decode(&req)
	if req.AgentID != 0 && req.AgentID != ag.ExternalID {
		http.Error(w, "agent mismatch", http.StatusUnauthorized)
		return
	}

	clientIP := parseIP(r)

	_, err := a.pool.Exec(r.Context(),
		`update agents set last_seen=now(), version=coalesce(nullif($1,''),version),
                        location=coalesce(nullif($2,''),location), agent_ip=$3 where id=$4`,
		req.Version, req.Location, clientIP, ag.ID)
	if err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}
	ag.IP = net.ParseIP(clientIP)
	resp := heartbeatResp{
		AgentID:        ag.ExternalID,
		NextDeadlineMs: time.Now().Add(a.heartbeatTimeout()).UnixMilli(),
	}
	_ = json.NewEncoder(w).Encode(resp)

	if a.shouldEmitAgentOnline(r.Context(), ag.ID) {
		a.publishAgentOnline(r.Context(), ag.ID, ag.ExternalID, ag.Name, clientIP, req.Location, req.Version)
	}
}

/*************** Claim (выдача задач) ***************/
func (a *App) handleClaim(w http.ResponseWriter, r *http.Request) {
	ag := agentFromCtx(r.Context())
	if ag == nil {
		http.Error(w, "unauthorized", http.StatusUnauthorized)
		return
	}
	var req claimReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil && !errors.Is(err, io.EOF) {
		http.Error(w, "bad json", http.StatusBadRequest)
		return
	}
	if req.AgentID != 0 && req.AgentID != ag.ExternalID {
		http.Error(w, "agent mismatch", http.StatusUnauthorized)
		return
	}
	// лимит параллельных задач
	var active int
	if err := a.pool.QueryRow(r.Context(),
		`select count(1) from leases where agent_id=$1 and leased_until>now()`,
		ag.ID).Scan(&active); err == nil && active >= ag.Limit {
		w.WriteHeader(http.StatusNoContent)
		return
	}

	requested := 0
	for _, cap := range req.Capacities {
		if cap > 0 {
			requested += cap
		}
	}
	remaining := ag.Limit - active
	if requested == 0 || requested > remaining {
		requested = remaining
	}
	if requested <= 0 {
		w.WriteHeader(http.StatusNoContent)
		return
	}

	block := time.Duration(a.cfg.ClaimBlockMs) * time.Millisecond
	args := &redis.XReadArgs{Streams: []string{a.cfg.StreamTasks, "$"}, Count: int64(requested), Block: block}
	res, err := a.redis.XRead(r.Context(), args).Result()
	if err != nil && !errors.Is(err, redis.Nil) {
		http.Error(w, "queue error", http.StatusBadGateway)
		return
	}
	if len(res) == 0 || len(res[0].Messages) == 0 {
		w.WriteHeader(http.StatusNoContent)
		return
	}

	messages := res[0].Messages
	a.observeStreamRead(r.Context(), a.cfg.StreamTasks, requested, messages)

	var leases []leaseDTO
	for _, msg := range messages {
		taskID := uuidFromAny(msg.Values["task_id"])
		checkID := uuidFromAny(msg.Values["check_id"])
		kind := strFromAny(msg.Values["kind"])
		spec := []byte(strFromAny(msg.Values["spec"]))
		if taskID == uuid.Nil || checkID == uuid.Nil || kind == "" || len(spec) == 0 {
			continue
		}
		lockKey := leaseKey(msg.ID)
		ok, err := a.redis.SetNX(r.Context(), lockKey, ag.ID.String(), a.cfg.LeaseTTL).Result()
		if err != nil || !ok {
			continue
		}
		var (
			leaseID     uuid.UUID
			leaseExtID  int64
			leasedUntil time.Time
		)
		err = a.pool.QueryRow(r.Context(),
			`insert into leases(agent_id,check_id,task_id,kind,spec,stream_id,leased_until)
                         values($1,$2,$3,$4,$5,$6,now()+$7::interval) returning id, external_id, leased_until`,
			ag.ID, checkID, taskID, kind, spec, msg.ID, fmt.Sprintf("%f seconds", a.cfg.LeaseTTL.Seconds())).
			Scan(&leaseID, &leaseExtID, &leasedUntil)
		if err != nil {
			_ = a.redis.Del(r.Context(), lockKey).Err()
			continue
		}

		// Публикуем минимальное событие "check.start" для карты
		a.publishMapStart(r.Context(), checkID, ag, kind, spec)

		leases = append(leases, leaseDTO{
			LeaseID:      uint64(leaseExtID),
			TaskID:       uuidToUint64(taskID),
			Kind:         kind,
			LeaseUntilMs: leasedUntil.UTC().UnixMilli(),
			Spec:         json.RawMessage(spec),
		})
		if len(leases) >= requested {
			break
		}
	}

	if len(leases) == 0 {
		w.WriteHeader(http.StatusNoContent)
		return
	}

	_ = json.NewEncoder(w).Encode(claimResp{Leases: leases})
}

/*************** Extend (продление аренды) ***************/
func (a *App) handleExtend(w http.ResponseWriter, r *http.Request) {
	ag := agentFromCtx(r.Context())
	if ag == nil {
		http.Error(w, "unauthorized", http.StatusUnauthorized)
		return
	}

	var req extendReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "bad json", http.StatusBadRequest)
		return
	}
	if req.AgentID != 0 && req.AgentID != ag.ExternalID {
		http.Error(w, "agent mismatch", http.StatusUnauthorized)
		return
	}
	if len(req.LeaseIDs) == 0 {
		http.Error(w, "no leases to extend", http.StatusBadRequest)
		return
	}
	if req.ExtendByMs == 0 {
		http.Error(w, "invalid extend", http.StatusBadRequest)
		return
	}

	extendBy := time.Duration(req.ExtendByMs) * time.Millisecond
	outcomes := make([]extendOutcome, 0, len(req.LeaseIDs))

	for _, leaseExtID := range req.LeaseIDs {
		var (
			leaseID     uuid.UUID
			streamID    string
			leasedUntil time.Time
		)
		err := a.pool.QueryRow(r.Context(),
			`select id, stream_id, leased_until from leases where external_id=$1 and agent_id=$2`,
			leaseExtID, ag.ID).
			Scan(&leaseID, &streamID, &leasedUntil)
		if err != nil {
			continue
		}

		now := time.Now()
		if leasedUntil.Before(now) {
			leasedUntil = now
		}
		deadline := leasedUntil.Add(extendBy)
		if deadline.Before(now) {
			continue
		}

		deadline = deadline.UTC()
		if _, err := a.pool.Exec(r.Context(),
			`update leases set leased_until=$1 where id=$2`, deadline, leaseID); err != nil {
			continue
		}
		if err := a.redis.ExpireAt(r.Context(), leaseKey(streamID), deadline).Err(); err != nil {
			continue
		}

		outcomes = append(outcomes, extendOutcome{
			LeaseID:       leaseExtID,
			NewDeadlineMs: deadline.UnixMilli(),
		})
	}

	_ = json.NewEncoder(w).Encode(extendResp{Outcomes: outcomes})
}

/*************** Report (результаты) ***************/
func (a *App) handleReport(w http.ResponseWriter, r *http.Request) {
	ag := agentFromCtx(r.Context())
	if ag == nil {
		http.Error(w, "unauthorized", http.StatusUnauthorized)
		return
	}
	var req reportReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "bad json", http.StatusBadRequest)
		return
	}
	if req.AgentID != 0 && req.AgentID != ag.ExternalID {
		http.Error(w, "agent mismatch", http.StatusUnauthorized)
		return
	}
	if len(req.Completed) == 0 && len(req.Cancelled) == 0 {
		http.Error(w, "empty report", http.StatusBadRequest)
		return
	}

	acknowledged := 0

	quickMetaCache := map[uuid.UUID]*quickCheckMeta{}
	fetchQuickMeta := func(id uuid.UUID) *quickCheckMeta {
		if meta, ok := quickMetaCache[id]; ok {
			return meta
		}
		meta, err := a.quickCacheMeta(r.Context(), id)
		if err != nil {
			a.log.Warn("quick_meta_lookup_failed", "check_id", id, "err", err)
			quickMetaCache[id] = nil
			return nil
		}
		quickMetaCache[id] = meta
		return meta
	}

	process := func(entries []leaseReport, status string) {
		for _, c := range entries {
			var checkID uuid.UUID
			var taskID uuid.UUID
			var kind string
			var spec []byte
			var streamID string
			err := a.pool.QueryRow(r.Context(),
				`select check_id, task_id, kind, spec, stream_id from leases where external_id=$1 and agent_id=$2`,
				c.LeaseID, ag.ID).
				Scan(&checkID, &taskID, &kind, &spec, &streamID)
			if err != nil {
				continue
			}

			var siteStr sql.NullString
			var reqIP sql.NullString
			_ = a.pool.QueryRow(r.Context(),
				`select site_id::text, request_ip::text from checks where id=$1`, checkID).Scan(&siteStr, &reqIP)
			var siteID uuid.UUID
			if siteStr.Valid {
				if parsed, err := uuid.Parse(siteStr.String); err == nil {
					siteID = parsed
				}
			}
			_, _ = a.pool.Exec(r.Context(),
				`update checks set started_at = coalesce(started_at, now()) where id=$1`, checkID)

			payload := observationsPayload(c.Observations)
			mergedMetrics := mergeJSON(json.RawMessage(nil), map[string]any{
				"agent_id": ag.ID.String(),
				"agent_ip": parseIP(r),
			})

			var resID uuid.UUID
			err = a.pool.QueryRow(r.Context(),
				`insert into check_results(check_id, kind, status, payload, metrics, stream_id)
                                 values ($1,$2,$3,$4,$5,$6) returning id`,
				checkID, kind, status, json.RawMessage(payload), mergedMetrics, streamID).Scan(&resID)
			if err != nil {
				a.log.Error("insert_result", "err", err)
				continue
			}

			var cacheKey string
			if siteStr.Valid {
				cacheKey = fmt.Sprintf("recent:%s:%s", siteStr.String, kind)
			} else if meta := fetchQuickMeta(checkID); meta != nil {
				cacheKey = fmt.Sprintf("recent:%s:%s", meta.CacheKey, kind)
			}
			if cacheKey != "" {
				cacheVal := map[string]any{
					"check_id": checkID.String(),
					"kind":     kind,
					"status":   status,
					"payload":  json.RawMessage(payload),
					"metrics":  json.RawMessage(mergedMetrics),
					"ts":       time.Now().UTC().Format(time.RFC3339Nano),
				}
				if b, err := json.Marshal(cacheVal); err == nil {
					_ = a.redis.Set(r.Context(), cacheKey, string(b), a.cfg.CacheTTL).Err()
				}
			}

			_ = a.publishUpdate(r.Context(), checkID, "result", map[string]any{
				"check_id": checkID, "kind": kind, "status": status,
				"payload":  json.RawMessage(payload),
				"metrics":  json.RawMessage(mergedMetrics),
				"agent_id": ag.ID, "task_id": taskID,
			})

			a.publishMapResult(r.Context(), checkID, siteID, reqIP.String, ag, kind, spec, status, json.RawMessage(payload), mergedMetrics, "")

			_, _ = a.pool.Exec(r.Context(), `delete from leases where external_id=$1`, c.LeaseID)
			_ = a.redis.Del(r.Context(), leaseKey(streamID)).Err()
			_, _ = a.redis.XDel(r.Context(), a.cfg.StreamTasks, streamID).Result()

			var pending int
			_ = a.pool.QueryRow(r.Context(), `select count(1) from leases where check_id=$1`, checkID).Scan(&pending)
			if pending == 0 {
				_, _ = a.pool.Exec(r.Context(),
					`update checks set finished_at=coalesce(finished_at, now()),
                                         status = case when status='error' then status else 'done' end
                                         where id=$1`, checkID)
				_ = a.publishUpdate(r.Context(), checkID, "done", map[string]any{"check_id": checkID})
				a.publishMapDone(r.Context(), checkID)
			}

			acknowledged++
		}
	}

	process(req.Completed, "ok")
	process(req.Cancelled, "cancelled")

	_ = json.NewEncoder(w).Encode(reportResp{Acknowledged: acknowledged})
}

/*************** Retry loop (lease expiration) ***************/
func (a *App) retryLoop() {
	t := time.NewTicker(3 * time.Second)
	defer t.Stop()
	ctx := context.Background()

	for range t.C {
		rows, err := a.pool.Query(ctx,
			`select id, agent_id, check_id, task_id, kind, spec, stream_id, retry_count
			   from leases
			  where leased_until < now()`)
		if err != nil {
			continue
		}
		type rec struct {
			ID, CheckID, TaskID uuid.UUID
			Kind                string
			Spec                []byte
			StreamID            string
			Retry               int
		}
		var todo []rec
		for rows.Next() {
			var it struct {
				ID, AgentID, CheckID, TaskID uuid.UUID
				Kind                         string
				Spec                         []byte
				StreamID                     string
				Retry                        int
			}
			if err := rows.Scan(&it.ID, &it.AgentID, &it.CheckID, &it.TaskID, &it.Kind, &it.Spec, &it.StreamID, &it.Retry); err == nil {
				todo = append(todo, rec{ID: it.ID, CheckID: it.CheckID, TaskID: it.TaskID, Kind: it.Kind, Spec: it.Spec, StreamID: it.StreamID, Retry: it.Retry})
			}
		}
		rows.Close()

		for _, it := range todo {
			var leasedUntil time.Time
			if err := a.pool.QueryRow(ctx, `select leased_until from leases where id=$1`, it.ID).Scan(&leasedUntil); err != nil {
				continue
			}
			now := time.Now()
			if leasedUntil.After(now) {
				continue
			}
			if it.Retry >= a.cfg.MaxRetries {
				_, _ = a.pool.Exec(ctx, `delete from leases where id=$1 and leased_until < now()`, it.ID)
				_, _ = a.pool.Exec(ctx,
					`insert into check_results(check_id, kind, status, payload, metrics, stream_id)
					 values ($1,$2,'cancelled','{}',NULL,$3)`, it.CheckID, it.Kind, it.StreamID)
				_ = a.publishUpdate(ctx, it.CheckID, "result", map[string]any{
					"check_id": it.CheckID, "kind": it.Kind, "status": "cancelled", "reason": "lease_timeout", "retries": it.Retry,
				})
				var siteID uuid.UUID
				var reqIP sql.NullString
				var siteStr sql.NullString
				_ = a.pool.QueryRow(ctx,
					`select site_id::text, request_ip::text from checks where id=$1`, it.CheckID).
					Scan(&siteStr, &reqIP)
				if siteStr.Valid {
					if parsed, err := uuid.Parse(siteStr.String); err == nil {
						siteID = parsed
					}
				}
				a.publishMapResult(ctx, it.CheckID, siteID, reqIP.String, nil, it.Kind, it.Spec,
					"cancelled", json.RawMessage(`{}`), nil, "lease_timeout")
				continue
			}
			time.Sleep(a.cfg.RetryBackoff)
			val := map[string]any{
				"task_id":  it.TaskID.String(),
				"check_id": it.CheckID.String(),
				"kind":     it.Kind,
				"spec":     string(it.Spec),
			}
			if id, err := a.redis.XAdd(ctx, &redis.XAddArgs{Stream: a.cfg.StreamTasks, Values: val}).Result(); err == nil {
				a.observeStreamWrite(ctx, a.cfg.StreamTasks, id, "requeue")
				_, _ = a.pool.Exec(ctx, `delete from leases where id=$1 and leased_until < now()`, it.ID)
				_ = a.redis.Del(ctx, leaseKey(it.StreamID)).Err()
				_, _ = a.redis.XDel(ctx, a.cfg.StreamTasks, it.StreamID).Result()
			}
		}
	}
}

/*************** SSE checks (как было) ***************/
func (a *App) publishUpdate(ctx context.Context, checkID uuid.UUID, typ string, data any) error {
	env := map[string]any{"type": typ, "ts": time.Now().UTC().Format(time.RFC3339Nano), "data": data}
	b, _ := json.Marshal(env)
	channel := a.cfg.PubPrefix + checkID.String()
	return a.redis.Publish(ctx, channel, string(b)).Err()
}

/*************** GEO / Map helpers ***************/
type Geo struct {
	Lat     float64 `json:"lat"`
	Lon     float64 `json:"lon"`
	Country string  `json:"country,omitempty"`
	City    string  `json:"city,omitempty"`
	ASN     int     `json:"asn,omitempty"`
}
type cityDB interface {
	City(net.IP) (*geoip2.City, error)
	Close() error
}

type asnDB interface {
	ASN(net.IP) (*geoip2.ASN, error)
	Close() error
}

type geoIP struct {
	city cityDB
	asn  asnDB
}

type geoCacheEntry struct {
	value   *Geo
	ok      bool
	expires time.Time
}

type geoCache struct {
	ttl   time.Duration
	mu    sync.RWMutex
	items map[string]geoCacheEntry
}

func newGeoCache(ttl time.Duration) *geoCache {
	if ttl <= 0 {
		return nil
	}
	return &geoCache{ttl: ttl, items: make(map[string]geoCacheEntry)}
}

func (c *geoCache) get(ip string) (*Geo, bool, bool) {
	if c == nil {
		return nil, false, false
	}
	c.mu.RLock()
	entry, ok := c.items[ip]
	c.mu.RUnlock()
	if !ok {
		return nil, false, false
	}
	if time.Now().After(entry.expires) {
		c.mu.Lock()
		delete(c.items, ip)
		c.mu.Unlock()
		return nil, false, false
	}
	if entry.value == nil {
		return nil, entry.ok, true
	}
	clone := *entry.value
	return &clone, entry.ok, true
}

func (c *geoCache) set(ip string, value *Geo, ok bool) {
	if c == nil {
		return
	}
	var stored *Geo
	if value != nil {
		clone := *value
		stored = &clone
	}
	c.mu.Lock()
	c.items[ip] = geoCacheEntry{value: stored, ok: ok, expires: time.Now().Add(c.ttl)}
	c.mu.Unlock()
}

func openGeo(cityPath, asnPath string) (*geoIP, error) {
	g := &geoIP{}
	if cityPath != "" {
		reader, err := geoip2.Open(cityPath)
		if err != nil {
			return nil, err
		}
		g.city = reader
	}
	if asnPath != "" {
		if reader, err := geoip2.Open(asnPath); err == nil {
			g.asn = reader
		}
	}
	return g, nil
}

func (g *geoIP) Close() {
	if g == nil {
		return
	}
	if g.city != nil {
		_ = g.city.Close()
	}
	if g.asn != nil {
		_ = g.asn.Close()
	}
}

func (a *App) geoLookup(ipStr string) (*Geo, bool) {
	ipStr = strings.TrimSpace(ipStr)
	if a.geo == nil || ipStr == "" {
		return nil, false
	}
	if cached, ok, found := a.geoCache.get(ipStr); found {
		return cached, ok
	}
	ip := net.ParseIP(ipStr)
	if ip == nil {
		if a.geoCache != nil {
			a.geoCache.set(ipStr, nil, false)
		}
		return nil, false
	}
	out := &Geo{}
	var success bool
	if a.geo.city != nil {
		if rec, err := a.geo.city.City(ip); err == nil {
			success = true
			if rec.Location.Latitude != 0 || rec.Location.Longitude != 0 {
				out.Lat = rec.Location.Latitude
				out.Lon = rec.Location.Longitude
			}
			if rec.Country.IsoCode != "" {
				out.Country = rec.Country.IsoCode
			}
			if len(rec.City.Names) > 0 {
				out.City = rec.City.Names["en"]
			}
		}
	}
	if a.geo.asn != nil {
		if asn, err := a.geo.asn.ASN(ip); err == nil {
			success = true
			out.ASN = int(asn.AutonomousSystemNumber)
		}
	}
	if !success {
		if a.geoCache != nil {
			a.geoCache.set(ipStr, nil, false)
		}
		return nil, false
	}
	if a.geoCache != nil {
		a.geoCache.set(ipStr, out, true)
	}
	return out, true
}

/*************** Map events ***************/
type mapEvent struct {
	Type string         `json:"type"` // check.start|check.result|check.done|agent.online
	TS   string         `json:"ts"`
	Data map[string]any `json:"data"`
}

func agentOnlineKey(id uuid.UUID) string {
	return "agent:online:event:" + id.String()
}

func (a *App) markAgentOnlineSent(ctx context.Context, id uuid.UUID) {
	if a.redis == nil {
		return
	}
	if err := a.redis.Set(ctx, agentOnlineKey(id), "1", agentOnlineEventTTL).Err(); err != nil && a.log != nil {
		a.log.Warn("agent_online_event_mark_failed", "err", err)
	}
}

func (a *App) shouldEmitAgentOnline(ctx context.Context, id uuid.UUID) bool {
	if a.redis == nil {
		return true
	}
	ok, err := a.redis.SetNX(ctx, agentOnlineKey(id), "1", agentOnlineEventTTL).Result()
	if err != nil {
		if a.log != nil {
			a.log.Warn("agent_online_event_rate_limit", "err", err)
		}
		return true
	}
	return ok
}

func (a *App) publishAgentOnline(ctx context.Context, id uuid.UUID, externalID uint64, name, ip, location, version string) {
	agent := map[string]any{
		"id":          id,
		"external_id": externalID,
		"name":        name,
		"status":      "online",
	}
	ip = strings.TrimSpace(ip)
	if ip != "" {
		agent["ip"] = ip
		if geo := geoJSON(a, ip); geo != nil {
			agent["geo"] = geo
		}
	}
	if loc := strings.TrimSpace(location); loc != "" {
		agent["location"] = loc
	}
	if ver := strings.TrimSpace(version); ver != "" {
		agent["version"] = ver
	}
	data := map[string]any{
		"status": "online",
		"agent":  agent,
	}
	a.publishMapEvent(ctx, mapEvent{Type: "agent.online", Data: data})
}

func (a *App) publishMapEvent(ctx context.Context, ev mapEvent) {
	ev.TS = time.Now().UTC().Format(time.RFC3339Nano)
	b, _ := json.Marshal(ev)

	// Pub/Sub
	_ = a.redis.Publish(ctx, a.cfg.MapPubChannel, string(b)).Err()

	// Stream (история): MAXLEN ~ a.cfg.MapStreamMaxLen
	if id, err := a.redis.XAdd(ctx, &redis.XAddArgs{
		Stream: a.cfg.MapStream,
		MaxLen: int64(a.cfg.MapStreamMaxLen),
		Approx: true,
		Values: map[string]any{"json": string(b)},
	}).Result(); err != nil {
		a.log.Warn("map_stream_append_failed", "err", err)
	} else {
		a.observeStreamWrite(ctx, a.cfg.MapStream, id, ev.Type)
	}
}

func streamIDTime(id string) (time.Time, bool) {
	parts := strings.SplitN(id, "-", 2)
	if len(parts) == 0 {
		return time.Time{}, false
	}
	ms, err := strconv.ParseInt(parts[0], 10, 64)
	if err != nil {
		return time.Time{}, false
	}
	return time.UnixMilli(ms), true
}

func (a *App) observeStreamRead(ctx context.Context, stream string, requested int, messages []redis.XMessage) {
	if len(messages) == 0 {
		return
	}
	now := time.Now()
	var (
		totalLag time.Duration
		maxLag   time.Duration
		counted  int
	)
	for _, msg := range messages {
		if ts, ok := streamIDTime(msg.ID); ok {
			lag := now.Sub(ts)
			if lag < 0 {
				lag = 0
			}
			totalLag += lag
			if lag > maxLag {
				maxLag = lag
			}
			counted++
		}
	}
	avgLagMs := int64(0)
	if counted > 0 {
		avgLagMs = (totalLag / time.Duration(counted)).Milliseconds()
	}
	length, err := a.redis.XLen(ctx, stream).Result()
	if err != nil {
		a.log.Warn("stream_read_metrics", "stream", stream, "err", err)
	}
	a.log.Info("stream_read", "stream", stream, "requested", requested, "delivered", len(messages),
		"lag_max_ms", maxLag.Milliseconds(), "lag_avg_ms", avgLagMs, "stream_length", length)
}

func (a *App) observeStreamWrite(ctx context.Context, stream, id, source string) {
	length, err := a.redis.XLen(ctx, stream).Result()
	if err != nil {
		a.log.Warn("stream_write_metrics", "stream", stream, "source", source, "err", err)
		return
	}
	var lagMs int64
	if ts, ok := streamIDTime(id); ok {
		lag := time.Since(ts)
		if lag < 0 {
			lag = 0
		}
		lagMs = lag.Milliseconds()
	}
	a.log.Info("stream_write", "stream", stream, "source", source, "id", id, "stream_length", length, "lag_ms", lagMs)
}

func (a *App) publishMapStart(ctx context.Context, checkID uuid.UUID, ag *agentAuth, kind string, spec []byte) {
	// request_ip и site_id
	var siteID uuid.UUID
	var reqIP sql.NullString
	var siteStr sql.NullString
	_ = a.pool.QueryRow(ctx, `select site_id::text, request_ip::text from checks where id=$1`, checkID).Scan(&siteStr, &reqIP)
	if siteStr.Valid {
		if parsed, err := uuid.Parse(siteStr.String); err == nil {
			siteID = parsed
		}
	}

	targetHost, _ := extractTargetFromSpec(kind, spec)
	data := map[string]any{
		"check_id": checkID,
		"kind":     kind,
		"status":   "running",
	}
	if siteID != uuid.Nil {
		data["site_id"] = siteID
	}
	source := map[string]any{}
	if reqIP.String != "" {
		source["ip"] = reqIP.String
		if geo := geoJSON(a, reqIP.String); geo != nil {
			source["geo"] = geo
		}
	}
	if len(source) > 0 {
		data["source"] = source
	}
	if ag != nil {
		agent := map[string]any{
			"id":          ag.ID,
			"external_id": ag.ExternalID,
			"name":        ag.Name,
		}
		if aip := ipString(ag.IP); aip != "" {
			agent["ip"] = aip
			if geo := geoJSON(a, aip); geo != nil {
				agent["geo"] = geo
			}
		}
		data["agent"] = agent
	}
	target := map[string]any{}
	if targetHost != "" {
		target["host"] = targetHost
		if looksIP(targetHost) {
			target["ip"] = targetHost
			if geo := geoJSON(a, targetHost); geo != nil {
				target["geo"] = geo
			}
		}
	}
	if len(target) > 0 {
		data["target"] = target
	}
	a.publishMapEvent(ctx, mapEvent{Type: "check.start", Data: data})
}

func (a *App) publishMapResult(ctx context.Context, checkID, siteID uuid.UUID, reqIP string, ag *agentAuth,
	kind string, spec []byte, status string, payload json.RawMessage, metrics []byte, reason string) {

	targetHost, _ := extractTargetFromSpec(kind, spec)
	targetIP := findTargetIP(kind, payload, metrics)

	var trace []map[string]any
	if kind == "trace" {
		trace = findTrace(payload)
		for i := range trace {
			if ip, _ := trace[i]["ip"].(string); ip != "" {
				trace[i]["geo"] = geoJSON(a, ip)
			}
		}
	}

	data := map[string]any{
		"check_id": checkID,
		"kind":     kind,
		"status":   status,
	}
	if siteID != uuid.Nil {
		data["site_id"] = siteID
	}
	source := map[string]any{}
	if strings.TrimSpace(reqIP) != "" {
		source["ip"] = reqIP
		if geo := geoJSON(a, reqIP); geo != nil {
			source["geo"] = geo
		}
	}
	if len(source) > 0 {
		data["source"] = source
	}
	target := map[string]any{}
	if targetHost != "" {
		target["host"] = targetHost
		if looksIP(targetHost) {
			target["ip"] = targetHost
			if geo := geoJSON(a, targetHost); geo != nil {
				target["geo"] = geo
			}
		}
	}
	if targetIP != "" {
		target["ip"] = targetIP
		if geo := geoJSON(a, targetIP); geo != nil {
			target["geo"] = geo
		}
	}
	if len(target) > 0 {
		data["target"] = target
	}
	if len(trace) > 0 {
		data["trace"] = trace
	}
	if strings.TrimSpace(reason) != "" {
		data["reason"] = reason
	}
	if ag != nil {
		agent := map[string]any{
			"id":          ag.ID,
			"external_id": ag.ExternalID,
			"name":        ag.Name,
		}
		if aip := ipString(ag.IP); aip != "" {
			agent["ip"] = aip
			if geo := geoJSON(a, aip); geo != nil {
				agent["geo"] = geo
			}
		}
		data["agent"] = agent
	}
	a.publishMapEvent(ctx, mapEvent{Type: "check.result", Data: data})
}

func (a *App) publishMapDone(ctx context.Context, checkID uuid.UUID) {
	data := map[string]any{
		"check_id": checkID,
		"status":   "done",
	}
	if a.pool != nil {
		var siteID uuid.UUID
		var reqIP sql.NullString
		var checkStatus sql.NullString
		var siteStr sql.NullString
		if err := a.pool.QueryRow(ctx,
			`select site_id::text, request_ip::text, status from checks where id=$1`, checkID).
			Scan(&siteStr, &reqIP, &checkStatus); err == nil {
			if siteStr.Valid {
				if parsed, err := uuid.Parse(siteStr.String); err == nil {
					siteID = parsed
				}
			}
			if siteID != uuid.Nil {
				data["site_id"] = siteID
			}
			if strings.TrimSpace(checkStatus.String) != "" {
				data["status"] = checkStatus.String
			}
			if reqIP.String != "" {
				source := map[string]any{"ip": reqIP.String}
				if geo := geoJSON(a, reqIP.String); geo != nil {
					source["geo"] = geo
				}
				data["source"] = source
			}
		}
	}
	a.publishMapEvent(ctx, mapEvent{Type: "check.done", Data: data})
}

func geoJSON(a *App, ip string) any {
	if g, ok := a.geoLookup(ip); ok && g != nil {
		return g
	}
	return nil
}

/*************** Extractors ***************/
func extractTargetFromSpec(kind string, spec []byte) (host string, ok bool) {
	var m map[string]any
	if err := json.Unmarshal(spec, &m); err != nil {
		return "", false
	}
	get := func(keys ...string) (string, bool) {
		for _, k := range keys {
			if v, ok := m[k]; ok {
				if s, _ := v.(string); strings.TrimSpace(s) != "" {
					return s, true
				}
			}
		}
		return "", false
	}
	switch kind {
	case "http":
		if u, ok := get("url"); ok {
			return u, true
		}
	case "tcp":
		if h, ok := get("host"); ok {
			return h, true
		}
	case "dns":
		if q, ok := get("query"); ok {
			return q, true
		}
	case "trace":
		if h, ok := get("host"); ok {
			return h, true
		}
	case "ping":
		if h, ok := get("host"); ok {
			return h, true
		}
	}
	return "", false
}

func findTargetIP(kind string, payload json.RawMessage, metrics []byte) string {
	tryKeys := func(m map[string]any, keys ...string) string {
		for _, k := range keys {
			if v, ok := m[k]; ok {
				if s, _ := v.(string); looksIP(s) {
					return s
				}
			}
		}
		return ""
	}

	var mp map[string]any
	if len(metrics) > 0 {
		_ = json.Unmarshal(metrics, &mp)
		ip := tryKeys(mp, "resolved_ip", "target_ip", "connect_ip", "ip")
		if ip != "" {
			return ip
		}
	}
	var pl map[string]any
	if len(payload) > 0 {
		_ = json.Unmarshal(payload, &pl)
		if ip := tryKeys(pl, "resolved_ip", "target_ip", "connect_ip", "ip"); ip != "" {
			return ip
		}
		// dns answers
		if kind == "dns" {
			if ans, ok := pl["answers"].([]any); ok {
				for _, v := range ans {
					if row, _ := v.(map[string]any); row != nil {
						if ip := tryKeys(row, "data", "ip"); ip != "" {
							return ip
						}
					}
				}
			}
		}
	}
	return ""
}

func findTrace(payload json.RawMessage) []map[string]any {
	if len(payload) == 0 {
		return nil
	}
	var pl map[string]any
	if err := json.Unmarshal(payload, &pl); err != nil {
		return nil
	}
	var hops []map[string]any
	if arr, ok := pl["hops"].([]any); ok {
		for i, v := range arr {
			if m, _ := v.(map[string]any); m != nil {
				ip := ""
				if s, _ := m["ip"].(string); looksIP(s) {
					ip = s
				} else if s, _ := m["addr"].(string); looksIP(s) {
					ip = s
				}
				hops = append(hops, map[string]any{
					"n":  i + 1,
					"ip": ip,
				})
			}
		}
	}
	return hops
}

func looksIP(s string) bool {
	return net.ParseIP(strings.TrimSpace(s)) != nil
}

func ipString(ip net.IP) string {
	if ip == nil {
		return ""
	}
	return ip.String()
}

const (
	defaultMapAgentsMinutes   = 10
	maxMapAgentsMinutes       = 60
	defaultMapAgentsLimit     = 200
	maxMapAgentsLimit         = 500
	defaultMapSnapshotMinutes = 60
	maxMapSnapshotMinutes     = 1440
	defaultMapSnapshotLimit   = 1000
	maxMapSnapshotLimit       = 2000
	agentOnlineEventTTL       = 30 * time.Second
)

func clampIntParam(raw string, def, min, max int) int {
	val := def
	if v, err := strconv.Atoi(strings.TrimSpace(raw)); err == nil && v > 0 {
		val = v
	}
	if val < min {
		val = min
	}
	if max > 0 && val > max {
		val = max
	}
	return val
}

/*************** Endpoints: Map ***************/
func (a *App) handleMapAgents(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")

	q := r.URL.Query()
	minutes := clampIntParam(q.Get("minutes"), defaultMapAgentsMinutes, 1, maxMapAgentsMinutes)
	limit := clampIntParam(q.Get("limit"), defaultMapAgentsLimit, 1, maxMapAgentsLimit)

	rows, err := a.pool.Query(r.Context(),
		`select id, name, version, last_seen, max_parallel, agent_ip::text, coalesce(location,'')
                   from agents
                  where last_seen > now() - ($1::int * interval '1 minute')
                  order by last_seen desc
                  limit $2`, minutes, limit)
	if err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}
	defer rows.Close()

	type item struct {
		AgentID     uuid.UUID `json:"agent_id"`
		Name        string    `json:"name"`
		Version     string    `json:"version"`
		LastSeen    time.Time `json:"last_seen"`
		MaxParallel int       `json:"max_parallel"`
		IP          string    `json:"ip"`
		LocationStr string    `json:"location_str"`
		Geo         any       `json:"geo"`
	}
	var items []item
	for rows.Next() {
		var it item
		var ipStr string
		var loc string
		if err := rows.Scan(&it.AgentID, &it.Name, &it.Version, &it.LastSeen, &it.MaxParallel, &ipStr, &loc); err == nil {
			it.IP = ipStr
			it.LocationStr = loc
			it.Geo = geoJSON(a, ipStr)
			items = append(items, it)
		}
	}
	_ = json.NewEncoder(w).Encode(map[string]any{"items": items})
}

func (a *App) streamMapEvents(w http.ResponseWriter, r *http.Request) {
	// SSE
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")
	w.Header().Set("X-Accel-Buffering", "no")
	flusher, ok := w.(http.Flusher)
	if !ok {
		http.Error(w, "stream unsupported", http.StatusInternalServerError)
		return
	}
	w.WriteHeader(http.StatusOK)
	flusher.Flush()
	ctx := r.Context()
	pubsub := a.redis.Subscribe(ctx, a.cfg.MapPubChannel)
	defer pubsub.Close()
	ch := pubsub.Channel()

	ping := time.NewTicker(15 * time.Second)
	defer ping.Stop()

	for {
		select {
		case <-ctx.Done():
			return
		case <-ping.C:
			_, _ = io.WriteString(w, ": ping\n\n")
			flusher.Flush()
		case msg, ok := <-ch:
			if !ok {
				return
			}
			fmt.Fprintf(w, "event: update\n")
			fmt.Fprintf(w, "data: %s\n\n", msg.Payload)
			flusher.Flush()
		}
	}
}

func (a *App) handleMapStream(w http.ResponseWriter, r *http.Request) {
	a.streamMapEvents(w, r)
}

func (a *App) handleMapEvents(w http.ResponseWriter, r *http.Request) {
	token := strings.TrimSpace(r.URL.Query().Get("access_token"))
	if token != "" && strings.TrimSpace(r.Header.Get("Authorization")) == "" {
		clone := r.Clone(r.Context())
		clone.Header = clone.Header.Clone()
		clone.Header.Set("Authorization", "Bearer "+token)
		q := clone.URL.Query()
		q.Del("access_token")
		clone.URL.RawQuery = q.Encode()
		r = clone
	}
	a.streamMapEvents(w, r)
}

func (a *App) handleMapSnapshot(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	q := r.URL.Query()
	minutes := clampIntParam(q.Get("minutes"), defaultMapSnapshotMinutes, 1, maxMapSnapshotMinutes)
	limit := clampIntParam(q.Get("limit"), defaultMapSnapshotLimit, 1, maxMapSnapshotLimit)
	cursor := q.Get("cursor")

	var startID string
	if cursor != "" {
		startID = cursor
	} else {
		fromMs := time.Now().Add(-time.Duration(minutes) * time.Minute).UnixMilli()
		startID = fmt.Sprintf("%d-0", fromMs)
	}

	res, err := a.redis.XRangeN(r.Context(), a.cfg.MapStream, startID, "+", int64(limit)).Result()
	if err != nil && !errors.Is(err, redis.Nil) {
		http.Error(w, "stream error", http.StatusBadGateway)
		return
	}

	items := make([]json.RawMessage, 0, len(res))
	nextCursor := ""
	for _, m := range res {
		if nextCursor == "" || m.ID > nextCursor {
			nextCursor = m.ID
		}
		if s, ok := m.Values["json"].(string); ok && s != "" {
			items = append(items, json.RawMessage(s))
		}
	}
	_ = json.NewEncoder(w).Encode(map[string]any{"items": items, "next_cursor": nextCursor})
}

func (a *App) handleCheckGeo(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	idStr := chi.URLParam(r, "id")
	cid, err := uuid.Parse(idStr)
	if err != nil {
		http.Error(w, "bad id", http.StatusBadRequest)
		return
	}
	var reqIP sql.NullString
	_ = a.pool.QueryRow(r.Context(),
		`select request_ip::text from checks where id=$1`, cid).Scan(&reqIP)

	rows, err := a.pool.Query(r.Context(),
		`select kind, payload, metrics from check_results where check_id=$1 order by created_at asc`, cid)
	if err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}
	defer rows.Close()

	out := map[string]any{
		"check_id": cid,
		"source":   map[string]any{"ip": reqIP.String, "geo": geoJSON(a, reqIP.String)},
		"targets":  []map[string]any{},
		"trace":    map[string]any{"hops": []map[string]any{}},
	}

	for rows.Next() {
		var kind string
		var payload json.RawMessage
		var metrics []byte
		if err := rows.Scan(&kind, &payload, &metrics); err == nil {
			tIP := findTargetIP(kind, payload, metrics)
			target := map[string]any{"kind": kind}
			if tIP != "" {
				target["ip"] = tIP
				target["geo"] = geoJSON(a, tIP)
			}
			// попробуем host из metrics/payload
			var mp map[string]any
			_ = json.Unmarshal(metrics, &mp)
			if h, ok := mp["host"].(string); ok && h != "" {
				target["host"] = h
			}
			var pl map[string]any
			_ = json.Unmarshal(payload, &pl)
			if h, ok := pl["host"].(string); ok && h != "" && target["host"] == nil {
				target["host"] = h
			}
			// trace
			if kind == "trace" {
				if hops := findTrace(payload); hops != nil {
					for i := range hops {
						if ip, _ := hops[i]["ip"].(string); ip != "" {
							hops[i]["geo"] = geoJSON(a, ip)
						}
					}
					out["trace"] = map[string]any{"hops": hops}
				}
			}
			// агент (если записали в metrics)
			if agid, ok := mp["agent_id"].(string); ok && out["agent"] == nil {
				agent := map[string]any{"id": agid}
				if ip, ok := mp["agent_ip"].(string); ok {
					agent["ip"] = ip
					agent["geo"] = geoJSON(a, ip)
				}
				out["agent"] = agent
			}
			// Добавим target в список
			if kind != "trace" {
				cur := out["targets"].([]map[string]any)
				out["targets"] = append(cur, target)
			}
		}
	}
	_ = json.NewEncoder(w).Encode(out)
}

func (a *App) handleGeoLookup(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	ip := strings.TrimSpace(r.URL.Query().Get("ip"))
	if ip == "" {
		http.Error(w, "ip required", http.StatusBadRequest)
		return
	}
	resp := map[string]any{"ip": ip}
	if g, ok := a.geoLookup(ip); ok && g != nil {
		resp["geo"] = g
	}
	_ = json.NewEncoder(w).Encode(resp)
}

/*************** Utils ***************/
func leaseKey(streamID string) string { return "lease:" + streamID }

func uuidFromAny(v any) uuid.UUID {
	switch t := v.(type) {
	case string:
		id, _ := uuid.Parse(strings.TrimSpace(t))
		return id
	}
	return uuid.Nil
}
func strFromAny(v any) string {
	switch t := v.(type) {
	case string:
		return t
	case []byte:
		return string(t)
	}
	return ""
}
func nullJSON(b json.RawMessage) []byte {
	if len(b) == 0 || strings.TrimSpace(string(b)) == "" {
		return []byte(`{}`)
	}
	return b
}

func newRandomToken(n int) (string, error) {
	buf := make([]byte, n)
	if _, err := rand.Read(buf); err != nil {
		return "", err
	}
	return hex.EncodeToString(buf), nil
}

func mergeJSON(orig json.RawMessage, extra map[string]any) []byte {
	if len(orig) == 0 || strings.TrimSpace(string(orig)) == "" {
		b, _ := json.Marshal(extra)
		return b
	}
	var m map[string]any
	if err := json.Unmarshal(orig, &m); err != nil {
		return orig
	}
	for k, v := range extra {
		m[k] = v
	}
	b, _ := json.Marshal(m)
	return b
}

func normalizeRawValue(v json.RawMessage) json.RawMessage {
	if len(v) == 0 || strings.TrimSpace(string(v)) == "" {
		return json.RawMessage("null")
	}
	return v
}

func observationsPayload(obs []reportObservation) []byte {
	items := make([]map[string]any, 0, len(obs))
	for _, o := range obs {
		if strings.TrimSpace(o.Name) == "" {
			continue
		}
		entry := map[string]any{
			"name":  o.Name,
			"value": normalizeRawValue(o.Value),
		}
		if o.Unit != nil && strings.TrimSpace(*o.Unit) != "" {
			entry["unit"] = *o.Unit
		}
		items = append(items, entry)
	}
	payload := map[string]any{"observations": items}
	if len(items) == 0 {
		payload["observations"] = []any{}
	}
	b, _ := json.Marshal(payload)
	return b
}

func uuidToUint64(id uuid.UUID) uint64 {
	var buf [8]byte
	copy(buf[:], id[:8])
	return binary.BigEndian.Uint64(buf[:])
}

func (a *App) heartbeatTimeout() time.Duration {
	hb := time.Duration(a.cfg.ClaimBlockMs) * time.Millisecond
	if hb <= 0 {
		hb = a.cfg.LeaseTTL / 2
	}
	if hb <= 0 {
		hb = 30 * time.Second
	}
	return hb
}
