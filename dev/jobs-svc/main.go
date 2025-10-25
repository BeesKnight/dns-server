package main

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"database/sql"
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
	"syscall"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/go-chi/chi/v5/middleware"
	"github.com/google/uuid"
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

	// SSE/карта
	PubPrefix       string // для check-upd:* каналов (пер-чековые SSE)
	MapPubChannel   string // "map:events"
	MapStream       string // "map_events"
	MapStreamMaxLen int    // MaxLenApprox для истории

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

		PubPrefix:       env("PUB_PREFIX", "check-upd:"),
		MapPubChannel:   env("MAP_PUB_CHANNEL", "map:events"),
		MapStream:       env("MAP_STREAM", "map_events"),
		MapStreamMaxLen: ienv("MAP_STREAM_MAXLEN", 100000),

		GeoIPDisabled: benv("GEOIP_DISABLED", false),
		GeoIPCityPath: env("GEOIP_CITY_PATH", "/data/GeoLite2-City.mmdb"),
		GeoIPASNPath:  env("GEOIP_ASN_PATH", ""),
	}
}

/*************** App ***************/
type App struct {
	cfg   Config
	log   *slog.Logger
	pool  *pgxpool.Pool
	redis *redis.Client

	geo *geoIP // может быть nil (если выключено/файл не найден)
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
		rt.With(app.requireAgent).Post("/report", app.handleReport)
	})

	// карта
	r.Route("/v1", func(rt chi.Router) {
		rt.Get("/map/agents", app.handleMapAgents)
		rt.Get("/map/stream", app.handleMapStream)     // SSE
		rt.Get("/map/snapshot", app.handleMapSnapshot) // история из Stream
		rt.Get("/checks/{id}/geo", app.handleCheckGeo)
		rt.Get("/geo/lookup", app.handleGeoLookup)
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

	return &App{cfg: cfg, log: log, pool: pool, redis: rdb, geo: gip}, nil
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
func migrate(ctx context.Context, db *pgxpool.Pool) error {
	stmts := []string{
		`create extension if not exists "pgcrypto";`,
		`create table if not exists agents (
			id uuid primary key default gen_random_uuid(),
			name text not null,
			token_hash text not null,
			location text,
			version text,
			agent_ip inet,
			max_parallel int not null default 4,
			last_seen timestamptz,
			is_active boolean not null default true,
			created_at timestamptz not null default now()
		);`,
		`create index if not exists idx_agents_last_seen on agents(last_seen);`,

		`create table if not exists leases (
			id uuid primary key default gen_random_uuid(),
			agent_id uuid not null references agents(id) on delete cascade,
			check_id uuid not null,
			task_id uuid not null,
			kind text not null,
			spec jsonb not null,
			stream_id text not null,
			leased_until timestamptz not null,
			retry_count int not null default 0,
			created_at timestamptz not null default now()
		);`,
		`create index if not exists idx_leases_check on leases(check_id);`,
		`create index if not exists idx_leases_agent on leases(agent_id);`,
		`create index if not exists idx_leases_exp on leases(leased_until);`,
	}
	for _, s := range stmts {
		if _, err := db.Exec(ctx, s); err != nil {
			return err
		}
	}
	return nil
}

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
	ID    uuid.UUID
	Name  string
	IP    net.IP
	Limit int
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
		aid, err := uuid.Parse(idStr)
		if err != nil {
			http.Error(w, "bad agent id", http.StatusUnauthorized)
			return
		}
		var name string
		var tokenHash string
		var limit int
		err = a.pool.QueryRow(r.Context(),
			`select name, token_hash, max_parallel from agents where id=$1 and is_active=true`, aid).
			Scan(&name, &tokenHash, &limit)
		if err != nil {
			http.Error(w, "agent not found", http.StatusUnauthorized)
			return
		}
		if bcrypt.CompareHashAndPassword([]byte(tokenHash), []byte(tok)) != nil {
			http.Error(w, "invalid token", http.StatusUnauthorized)
			return
		}
		ip := net.ParseIP(parseIP(r))
		ctx := context.WithValue(r.Context(), agentAuthKey{}, &agentAuth{ID: aid, Name: name, IP: ip, Limit: limit})
		next.ServeHTTP(w, r.WithContext(ctx))
	})
}
func agentFromCtx(ctx context.Context) *agentAuth {
	v, _ := ctx.Value(agentAuthKey{}).(*agentAuth)
	return v
}

/*************** DTOs ***************/
type registerReq struct {
	Name        string `json:"name"`
	Location    string `json:"location,omitempty"`
	Version     string `json:"version,omitempty"`
	MaxParallel *int   `json:"max_parallel,omitempty"`
}
type registerResp struct {
	AgentID uuid.UUID `json:"AgentID"`
	Token   string    `json:"Token"`
}

type heartbeatReq struct {
	Status   string `json:"status,omitempty"`
	Version  string `json:"version,omitempty"`
	Location string `json:"location,omitempty"`
}

type claimResp struct {
	LeaseID      uuid.UUID       `json:"lease_id"`
	TaskID       uuid.UUID       `json:"task_id"`
	CheckID      uuid.UUID       `json:"check_id"`
	Kind         string          `json:"kind"`
	Spec         json.RawMessage `json:"spec"`
	LeaseUntilMs int64           `json:"lease_until_ms"`
	StreamMsgID  string          `json:"stream_msg_id"`
}

type reportReq struct {
	AgentID   uuid.UUID         `json:"agent_id"`
	Completed []reportCompleted `json:"completed"`
}
type reportCompleted struct {
	LeaseID      uuid.UUID       `json:"lease_id"`
	Status       string          `json:"status"` // ok|fail|cancelled
	Observations json.RawMessage `json:"observations"`
	Metrics      json.RawMessage `json:"metrics,omitempty"`
}

/*************** Handlers: register/heartbeat ***************/
func (a *App) handleAgentRegister(w http.ResponseWriter, r *http.Request) {
	var req registerReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil || strings.TrimSpace(req.Name) == "" {
		http.Error(w, "bad json", http.StatusBadRequest)
		return
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
	var id uuid.UUID
	err = a.pool.QueryRow(r.Context(),
		`insert into agents(name, token_hash, location, version, agent_ip, max_parallel, last_seen)
		 values($1,$2,$3,$4,$5,$6,now()) returning id`,
		req.Name, string(hash), req.Location, req.Version, parseIP(r), limit).Scan(&id)
	if err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}
	_ = json.NewEncoder(w).Encode(registerResp{AgentID: id, Token: plain})
}

func (a *App) handleHeartbeat(w http.ResponseWriter, r *http.Request) {
	ag := agentFromCtx(r.Context())
	if ag == nil {
		http.Error(w, "unauthorized", http.StatusUnauthorized)
		return
	}
	var req heartbeatReq
	_ = json.NewDecoder(r.Body).Decode(&req)

	_, err := a.pool.Exec(r.Context(),
		`update agents set last_seen=now(), version=coalesce(nullif($1,''),version),
			location=coalesce(nullif($2,''),location), agent_ip=$3 where id=$4`,
		req.Version, req.Location, parseIP(r), ag.ID)
	if err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

/*************** Claim (выдача задач) ***************/
func (a *App) handleClaim(w http.ResponseWriter, r *http.Request) {
	ag := agentFromCtx(r.Context())
	if ag == nil {
		http.Error(w, "unauthorized", http.StatusUnauthorized)
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

	block := time.Duration(a.cfg.ClaimBlockMs) * time.Millisecond
	args := &redis.XReadArgs{Streams: []string{a.cfg.StreamTasks, "$"}, Count: 10, Block: block}
	res, err := a.redis.XRead(r.Context(), args).Result()
	if err != nil && !errors.Is(err, redis.Nil) {
		http.Error(w, "queue error", http.StatusBadGateway)
		return
	}
	if len(res) == 0 || len(res[0].Messages) == 0 {
		w.WriteHeader(http.StatusNoContent)
		return
	}

	for _, msg := range res[0].Messages {
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
		var leaseID uuid.UUID
		err = a.pool.QueryRow(r.Context(),
			`insert into leases(agent_id,check_id,task_id,kind,spec,stream_id,leased_until)
			 values($1,$2,$3,$4,$5,$6,now()+$7::interval) returning id`,
			ag.ID, checkID, taskID, kind, spec, msg.ID, fmt.Sprintf("%f seconds", a.cfg.LeaseTTL.Seconds())).Scan(&leaseID)
		if err != nil {
			_ = a.redis.Del(r.Context(), lockKey).Err()
			continue
		}

		// Публикуем минимальное событие "check.start" для карты
		a.publishMapStart(r.Context(), checkID, ag, kind, spec)

		resp := claimResp{
			LeaseID:      leaseID,
			TaskID:       taskID,
			CheckID:      checkID,
			Kind:         kind,
			Spec:         json.RawMessage(spec),
			LeaseUntilMs: time.Now().Add(a.cfg.LeaseTTL).UnixMilli(),
			StreamMsgID:  msg.ID,
		}
		_ = json.NewEncoder(w).Encode(resp)
		return
	}

	w.WriteHeader(http.StatusNoContent)
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
	if req.AgentID != ag.ID {
		http.Error(w, "agent mismatch", http.StatusUnauthorized)
		return
	}
	if len(req.Completed) == 0 {
		http.Error(w, "empty report", http.StatusBadRequest)
		return
	}

	for _, c := range req.Completed {
		var checkID uuid.UUID
		var taskID uuid.UUID
		var kind string
		var spec []byte
		var streamID string
		err := a.pool.QueryRow(r.Context(),
			`select check_id, task_id, kind, spec, stream_id from leases where id=$1 and agent_id=$2`,
			c.LeaseID, ag.ID).
			Scan(&checkID, &taskID, &kind, &spec, &streamID)
		if err != nil {
			continue
		}

		// получим site_id и request_ip (нужно для карты и кэша)
		var siteID uuid.UUID
		var reqIP sql.NullString
		_ = a.pool.QueryRow(r.Context(),
			`select site_id, request_ip::text from checks where id=$1`, checkID).Scan(&siteID, &reqIP)
		_, _ = a.pool.Exec(r.Context(),
			`update checks set started_at = coalesce(started_at, now()) where id=$1`, checkID)

		// склеим метрики с агентской информацией
		mergedMetrics := mergeJSON(c.Metrics, map[string]any{
			"agent_id": ag.ID.String(),
			"agent_ip": parseIP(r),
		})

		// сохраняем результат
		var resID uuid.UUID
		err = a.pool.QueryRow(r.Context(),
			`insert into check_results(check_id, kind, status, payload, metrics, stream_id)
			 values ($1,$2,$3,$4,$5,$6) returning id`,
			checkID, kind, c.Status, nullJSON(c.Observations), mergedMetrics, streamID).Scan(&resID)
		if err != nil {
			a.log.Error("insert_result", "err", err)
			continue
		}

		// кэш последнего результата
		cacheKey := fmt.Sprintf("recent:%s:%s", siteID.String(), kind)
		cacheVal := map[string]any{
			"check_id": checkID.String(),
			"kind":     kind,
			"status":   c.Status,
			"payload":  json.RawMessage(nullJSON(c.Observations)),
			"metrics":  json.RawMessage(mergedMetrics),
			"ts":       time.Now().UTC().Format(time.RFC3339Nano),
		}
		if b, err := json.Marshal(cacheVal); err == nil {
			_ = a.redis.Set(r.Context(), cacheKey, string(b), a.cfg.CacheTTL).Err()
		}

		// Событие для SSE чеков (как было)
		_ = a.publishUpdate(r.Context(), checkID, "result", map[string]any{
			"check_id": checkID, "kind": kind, "status": c.Status,
			"payload":  json.RawMessage(nullJSON(c.Observations)),
			"metrics":  json.RawMessage(mergedMetrics),
			"agent_id": ag.ID, "task_id": taskID,
		})

		// Событие для карты (check.result)
		a.publishMapResult(r.Context(), checkID, siteID, reqIP.String, ag, kind, spec, c.Status, c.Observations, mergedMetrics)

		// очистка
		_, _ = a.pool.Exec(r.Context(), `delete from leases where id=$1`, c.LeaseID)
		_ = a.redis.Del(r.Context(), leaseKey(streamID)).Err()
		_, _ = a.redis.XDel(r.Context(), a.cfg.StreamTasks, streamID).Result()

		// если больше нет активных лиз — пометим done и отправим финал
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
	}

	w.WriteHeader(http.StatusNoContent)
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
			if it.Retry >= a.cfg.MaxRetries {
				_, _ = a.pool.Exec(ctx, `delete from leases where id=$1`, it.ID)
				_, _ = a.pool.Exec(ctx,
					`insert into check_results(check_id, kind, status, payload, metrics, stream_id)
					 values ($1,$2,'cancelled','{}',NULL,$3)`, it.CheckID, it.Kind, it.StreamID)
				_ = a.publishUpdate(ctx, it.CheckID, "result", map[string]any{
					"check_id": it.CheckID, "kind": it.Kind, "status": "cancelled", "reason": "lease_timeout", "retries": it.Retry,
				})
				a.publishMapEvent(ctx, mapEvent{
					Type: "check.result",
					Data: map[string]any{"check_id": it.CheckID, "kind": it.Kind, "status": "cancelled", "reason": "lease_timeout"},
				})
				continue
			}
			time.Sleep(a.cfg.RetryBackoff)
			val := map[string]any{
				"task_id":  it.TaskID.String(),
				"check_id": it.CheckID.String(),
				"kind":     it.Kind,
				"spec":     string(it.Spec),
			}
			if _, err := a.redis.XAdd(ctx, &redis.XAddArgs{Stream: a.cfg.StreamTasks, Values: val}).Result(); err == nil {
				_, _ = a.pool.Exec(ctx, `delete from leases where id=$1`, it.ID)
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
type geoIP struct {
	city *geoip2.Reader
	asn  *geoip2.Reader
}

func openGeo(cityPath, asnPath string) (*geoIP, error) {
	g := &geoIP{}
	var err error
	if cityPath != "" {
		if g.city, err = geoip2.Open(cityPath); err != nil {
			return nil, err
		}
	}
	if asnPath != "" {
		if g.asn, err = geoip2.Open(asnPath); err != nil {
			// не критично
		}
	}
	return g, nil
}
func (g *geoIP) Close() {
	if g.city != nil {
		g.city.Close()
	}
	if g.asn != nil {
		g.asn.Close()
	}
}

func (a *App) geoLookup(ipStr string) (*Geo, bool) {
	if a.geo == nil || strings.TrimSpace(ipStr) == "" {
		return nil, false
	}
	ip := net.ParseIP(ipStr)
	if ip == nil {
		return nil, false
	}
	out := &Geo{}
	if a.geo.city != nil {
		if rec, err := a.geo.city.City(ip); err == nil {
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
			out.ASN = int(asn.AutonomousSystemNumber)
		}
	}
	return out, true
}

/*************** Map events ***************/
type mapEvent struct {
	Type string         `json:"type"` // check.start|check.result|check.done|agent.online
	TS   string         `json:"ts"`
	Data map[string]any `json:"data"`
}

func (a *App) publishMapEvent(ctx context.Context, ev mapEvent) {
	ev.TS = time.Now().UTC().Format(time.RFC3339Nano)
	b, _ := json.Marshal(ev)

	// Pub/Sub
	_ = a.redis.Publish(ctx, a.cfg.MapPubChannel, string(b)).Err()

	// Stream (история): MAXLEN ~ a.cfg.MapStreamMaxLen
	_, _ = a.redis.XAdd(ctx, &redis.XAddArgs{
		Stream: a.cfg.MapStream,
		MaxLen: int64(a.cfg.MapStreamMaxLen),
		Approx: true,
		Values: map[string]any{"json": string(b)},
	}).Result()
}

func (a *App) publishMapStart(ctx context.Context, checkID uuid.UUID, ag *agentAuth, kind string, spec []byte) {
	// request_ip и site_id
	var siteID uuid.UUID
	var reqIP sql.NullString
	_ = a.pool.QueryRow(ctx, `select site_id, request_ip::text from checks where id=$1`, checkID).Scan(&siteID, &reqIP)

	targetHost, _ := extractTargetFromSpec(kind, spec)
	ev := mapEvent{
		Type: "check.start",
		Data: map[string]any{
			"check_id": checkID, "site_id": siteID, "kind": kind,
			"source": map[string]any{"ip": reqIP.String, "geo": geoJSON(a, reqIP.String)},
			"agent":  map[string]any{"id": ag.ID, "ip": ag.IP.String(), "geo": geoJSON(a, ag.IP.String())},
			"target": map[string]any{"host": targetHost},
		},
	}
	a.publishMapEvent(ctx, ev)
}

func (a *App) publishMapResult(ctx context.Context, checkID, siteID uuid.UUID, reqIP string, ag *agentAuth,
	kind string, spec []byte, status string, payload json.RawMessage, metrics []byte) {

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

	ev := mapEvent{
		Type: "check.result",
		Data: map[string]any{
			"check_id": checkID, "site_id": siteID, "kind": kind, "status": status,
			"source": map[string]any{"ip": reqIP, "geo": geoJSON(a, reqIP)},
			"agent":  map[string]any{"id": ag.ID, "ip": ag.IP.String(), "geo": geoJSON(a, ag.IP.String())},
			"target": map[string]any{"host": targetHost, "ip": targetIP, "geo": geoJSON(a, targetIP)},
			"trace":  trace,
		},
	}
	a.publishMapEvent(ctx, ev)
}

func (a *App) publishMapDone(ctx context.Context, checkID uuid.UUID) {
	a.publishMapEvent(ctx, mapEvent{
		Type: "check.done",
		Data: map[string]any{"check_id": checkID},
	})
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

/*************** Endpoints: Map ***************/
func (a *App) handleMapAgents(w http.ResponseWriter, r *http.Request) {
	rows, err := a.pool.Query(r.Context(),
		`select id, name, version, last_seen, max_parallel, agent_ip::text, coalesce(location,'')
		   from agents
		  where last_seen > now() - interval '10 minutes'
		  order by last_seen desc`)
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

func (a *App) handleMapStream(w http.ResponseWriter, r *http.Request) {
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

func (a *App) handleMapSnapshot(w http.ResponseWriter, r *http.Request) {
	q := r.URL.Query()
	minutes := 60
	if s := q.Get("minutes"); s != "" {
		if v, err := strconv.Atoi(s); err == nil && v > 0 {
			minutes = v
		}
	}
	limit := 1000
	if s := q.Get("limit"); s != "" {
		if v, err := strconv.Atoi(s); err == nil && v > 0 {
			limit = v
		}
	}
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
