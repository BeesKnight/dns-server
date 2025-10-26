package main

import (
	"context"
	"crypto/sha1"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"hash/fnv"
	"io"
	"log/slog"
	"math"
	"math/rand"
	"net"
	"net/http"
	"net/netip"
	"net/url"
	"os"
	"os/signal"
	"regexp"
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/go-chi/chi/v5/middleware"
	"github.com/google/uuid"
	"github.com/jackc/pgx/v5/pgxpool"
	"github.com/redis/go-redis/v9"
)

/*************** Config ***************/
type Config struct {
	HTTPAddr      string
	DatabaseURL   string
	RedisAddr     string
	RedisPassword string
	StreamTasks   string
	TaskTTL       time.Duration // дедуп спеков (короткое окно), НЕ TTL кэша результатов
}

func env(k, def string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return def
}

func parseTTL() time.Duration {
	if s := env("TASK_TTL_SECONDS", ""); s != "" {
		if d, err := time.ParseDuration(s + "s"); err == nil {
			return d
		}
	}
	return 60 * time.Second
}

func loadConfig() Config {
	return Config{
		HTTPAddr:      env("HTTP_ADDR", ":8081"),
		DatabaseURL:   env("DATABASE_URL", "postgres://postgres:dev@postgres:5432/aezacheck?sslmode=disable"),
		RedisAddr:     env("REDIS_ADDR", "redis:6379"),
		RedisPassword: env("REDIS_PASSWORD", ""),
		StreamTasks:   env("STREAM_TASKS", "check_tasks"),
		TaskTTL:       parseTTL(),
	}
}

/*************** App ***************/
type App struct {
	cfg   Config
	log   *slog.Logger
	pool  *pgxpool.Pool
	redis *redis.Client
}

/*************** Popular services catalog ***************/
type popularService struct {
	ID           string
	Name         string
	URL          string
	Status       string
	HourlyChecks []int
	BaseRating   float64
	BaseReviews  int
}

var (
	popularServices = []popularService{
		{
			ID:           "yt",
			Name:         "YouTube",
			URL:          "https://youtube.com",
			Status:       "ok",
			HourlyChecks: hourlySeries("yt", 1200, 2800),
			BaseRating:   4.7,
			BaseReviews:  2381,
		},
		{
			ID:           "gg",
			Name:         "Google",
			URL:          "https://google.com",
			Status:       "ok",
			HourlyChecks: hourlySeries("gg", 1000, 2500),
			BaseRating:   4.7,
			BaseReviews:  2210,
		},
		{
			ID:           "tg",
			Name:         "Telegram",
			URL:          "https://telegram.org",
			Status:       "ok",
			HourlyChecks: hourlySeries("tg", 900, 2200),
			BaseRating:   4.6,
			BaseReviews:  1975,
		},
		{
			ID:           "dc",
			Name:         "Discord",
			URL:          "https://discord.com",
			Status:       "warn",
			HourlyChecks: hourlySeries("dc", 600, 1500),
			BaseRating:   4.2,
			BaseReviews:  1120,
		},
		{
			ID:           "gh",
			Name:         "GitHub",
			URL:          "https://github.com",
			Status:       "ok",
			HourlyChecks: hourlySeries("gh", 400, 900),
			BaseRating:   4.9,
			BaseReviews:  980,
		},
	}

	serviceIndex = func() map[string]*popularService {
		m := make(map[string]*popularService, len(popularServices))
		for i := range popularServices {
			svc := &popularServices[i]
			m[svc.ID] = svc
		}
		return m
	}()
)

func hourlySeries(seed string, min, max int) []int {
	if max <= min {
		max = min + 1
	}
	base := fnv.New64a()
	_, _ = base.Write([]byte(seed))
	src := rand.New(rand.NewSource(int64(base.Sum64())))
	out := make([]int, 24)
	for i := range out {
		out[i] = min + src.Intn(max-min)
	}
	return out
}

func round1(f float64) float64 {
	return math.Round(f*10) / 10
}

func aggregateRatings(svc *popularService, stats reviewStats) (float64, int) {
	totalReviews := svc.BaseReviews + stats.Count
	if totalReviews <= 0 {
		return 0, 0
	}
	totalRating := svc.BaseRating*float64(svc.BaseReviews) + float64(stats.Sum)
	avg := totalRating / float64(totalReviews)
	return avg, totalReviews
}

func (a *App) fetchReviewStats(ctx context.Context, ids []string) (map[string]reviewStats, error) {
	stats := make(map[string]reviewStats, len(ids))
	for _, id := range ids {
		if strings.TrimSpace(id) == "" {
			continue
		}
		var cnt int64
		var sum int64
		if err := a.pool.QueryRow(ctx,
			`select count(*)::bigint, coalesce(sum(rating)::bigint, 0) from service_reviews where service_id=$1`,
			id,
		).Scan(&cnt, &sum); err != nil {
			return nil, err
		}
		stats[id] = reviewStats{Count: int(cnt), Sum: int(sum)}
	}
	return stats, nil
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

	r := chi.NewRouter()
	r.Use(middleware.RequestID, middleware.RealIP, middleware.Recoverer, middleware.Logger)
	// Важно: не ставим глобальный Timeout-мидлварь — помешает SSE

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

	// Sites CRUD + checks
	r.Route("/v1", func(rt chi.Router) {
		rt.Post("/sites", app.handleCreateSite)
		rt.Get("/sites", app.handleListSites)
		rt.Get("/sites/{id}", app.handleGetSite)
		rt.Delete("/sites/{id}", app.handleDeleteSite)

		rt.Route("/services", func(sr chi.Router) {
			sr.Get("/", app.handleListServices)
			sr.Get("/{id}/reviews", app.handleListServiceReviews)
			sr.Post("/{id}/reviews", app.handleCreateServiceReview)
		})

		rt.Post("/sites/{id}/checks", app.handleCreateChecks) // кэш → постановка задач
		rt.Get("/checks/{check_id}", app.handleGetCheck)      // view

		// SSE stream of updates for a check
		rt.Get("/checks/{check_id}/stream", app.handleStreamCheck)
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
	return &App{cfg: cfg, log: log, pool: pool, redis: rdb}, nil
}

func (a *App) Close() {
	if a.pool != nil {
		a.pool.Close()
	}
	if a.redis != nil {
		_ = a.redis.Close()
	}
}

/*************** Migrations (idempotent) ***************/
func migrate(ctx context.Context, db *pgxpool.Pool) error {
	stmts := []string{
		`create extension if not exists "pgcrypto";`,
		`create table if not exists users (
			id uuid primary key default gen_random_uuid(),
			email text unique not null,
			pass_hash text not null,
			role text not null default 'user',
			created_at timestamptz not null default now()
		);`,
		`create table if not exists sites (
			id uuid primary key default gen_random_uuid(),
			owner_id uuid references users(id) on delete cascade,
			name text,
			url text,
			host text,
			port int,
			dns_server inet,
			check_types text[] not null default '{}',
			created_at timestamptz not null default now()
		);`,
		`create table if not exists checks (
			id uuid primary key default gen_random_uuid(),
			site_id uuid references sites(id) on delete cascade,
			status text not null,                           -- queued|running|done|partial|error
			created_at timestamptz not null default now(),
			started_at timestamptz,
			finished_at timestamptz,
			created_by uuid references users(id),
			request_ip inet
		);`,
		`create table if not exists check_results (
                        id uuid primary key default gen_random_uuid(),
                        check_id uuid references checks(id) on delete cascade,
                        kind text not null,              -- http|ping|tcp|dns|trace
                        status text not null,            -- ok|cancelled|fail
                        payload jsonb not null,          -- observations[]
                        metrics jsonb,
                        stream_id text,
                        created_at timestamptz not null default now()
                );`,
		`create table if not exists service_reviews (
                        id uuid primary key default gen_random_uuid(),
                        service_id text not null,
                        user_id uuid not null references users(id) on delete cascade,
                        rating smallint not null check (rating between 1 and 5),
                        review text not null,
                        created_at timestamptz not null default now()
                );`,
		`create index if not exists idx_service_reviews_service on service_reviews(service_id);`,
	}
	for _, s := range stmts {
		if _, err := db.Exec(ctx, s); err != nil {
			return err
		}
	}
	return nil
}

/*************** Models / DTOs ***************/
type Site struct {
	ID         uuid.UUID   `json:"id"`
	OwnerID    *uuid.UUID  `json:"owner_id,omitempty"`
	Name       string      `json:"name"`
	URL        *string     `json:"url,omitempty"`
	Host       *string     `json:"host,omitempty"`
	Port       *int        `json:"port,omitempty"`
	DNSServer  *netip.Addr `json:"dns_server,omitempty"`
	CheckTypes []string    `json:"check_types"`
	CreatedAt  time.Time   `json:"created_at"`
}

type createSiteReq struct {
	Name       string   `json:"name"`
	URL        *string  `json:"url"`
	Host       *string  `json:"host"`
	Port       *int     `json:"port"`
	DNSServer  *string  `json:"dns_server"`
	CheckTypes []string `json:"check_types"` // ["http","ping","tcp","dns","trace"]
}

type listResp[T any] struct {
	Items []T `json:"items"`
}

type serviceListItem struct {
	ID           string  `json:"id"`
	Name         string  `json:"name"`
	URL          string  `json:"url"`
	Status       string  `json:"status"`
	Rating       float64 `json:"rating"`
	Reviews      int     `json:"reviews"`
	HourlyChecks []int   `json:"hourly_checks"`
}

type serviceReviewItem struct {
	ID        uuid.UUID `json:"id"`
	Rating    int       `json:"rating"`
	Text      string    `json:"text"`
	CreatedAt time.Time `json:"created_at"`
}

type serviceReviewsResp struct {
	Reviews       []serviceReviewItem `json:"reviews"`
	ReviewCount   int                 `json:"review_count"`
	AverageRating float64             `json:"average_rating"`
}

type createServiceReviewReq struct {
	Rating int    `json:"rating"`
	Text   string `json:"text"`
}

type createServiceReviewResp struct {
	Review        serviceReviewItem `json:"review"`
	ReviewCount   int               `json:"review_count"`
	AverageRating float64           `json:"average_rating"`
}

type reviewStats struct {
	Count int
	Sum   int
}

type createChecksReq struct {
	Types    []string                   `json:"types"`              // явный список видов
	Template string                     `json:"template,omitempty"` // full_site_health, network_deep, ...
	Args     map[string]json.RawMessage `json:"args,omitempty"`     // per-kind override
}

type createChecksResp struct {
	CheckID uuid.UUID `json:"check_id"`
	Queued  []string  `json:"queued"`
	Cached  []string  `json:"cached"`
	Skipped []string  `json:"skipped"`
}

type checkResult struct {
	ID        uuid.UUID       `json:"id"`
	Kind      string          `json:"kind"`
	Status    string          `json:"status"`
	Payload   json.RawMessage `json:"payload"`
	Metrics   json.RawMessage `json:"metrics"`
	StreamID  string          `json:"stream_id"`
	CreatedAt time.Time       `json:"created_at"`
}
type checkView struct {
	ID         uuid.UUID     `json:"id"`
	SiteID     uuid.UUID     `json:"site_id"`
	Status     string        `json:"status"`
	CreatedAt  time.Time     `json:"created_at"`
	StartedAt  *time.Time    `json:"started_at,omitempty"`
	FinishedAt *time.Time    `json:"finished_at,omitempty"`
	Results    []checkResult `json:"results"`
}

/*************** Helpers / validators ***************/
var hostRe = regexp.MustCompile(`^[a-zA-Z0-9\.\-]+$`)

func isHTTPURL(u string) bool {
	pu, err := url.Parse(u)
	return err == nil && (pu.Scheme == "http" || pu.Scheme == "https") && pu.Host != ""
}

func defaultPortForScheme(s string) int {
	switch strings.ToLower(s) {
	case "http":
		return 80
	case "https":
		return 443
	default:
		return 0
	}
}

func urlHost(u string) (string, int) {
	pu, err := url.Parse(u)
	if err != nil || pu.Host == "" {
		return "", 0
	}
	host, portStr, err := net.SplitHostPort(pu.Host)
	if err != nil { // порта нет
		return pu.Host, defaultPortForScheme(pu.Scheme)
	}
	if p, err := net.LookupPort("tcp", portStr); err == nil {
		return host, p
	}
	return host, 0
}

func sanitizeKinds(kinds []string) []string {
	seen := map[string]bool{}
	out := make([]string, 0, len(kinds))
	for _, k := range kinds {
		k = strings.ToLower(strings.TrimSpace(k))
		switch k {
		case "http", "ping", "tcp", "dns", "trace":
			if !seen[k] {
				seen[k] = true
				out = append(out, k)
			}
		}
	}
	return out
}

func parseDNSServer(s *string) (*netip.Addr, error) {
	if s == nil || *s == "" {
		return nil, nil
	}
	a, err := netip.ParseAddr(*s)
	if err != nil {
		return nil, fmt.Errorf("bad dns_server: %w", err)
	}
	return &a, nil
}

func hostFromSiteURL(s Site) string {
	if s.URL == nil || *s.URL == "" {
		return ""
	}
	h, _ := urlHost(*s.URL)
	if h == "" {
		pu, _ := url.Parse(*s.URL)
		return pu.Host
	}
	if strings.Contains(h, ":") {
		host, _, _ := net.SplitHostPort(h)
		return host
	}
	return h
}

func ptrStr(p *string) string {
	if p == nil {
		return ""
	}
	return *p
}

func firstNonEmpty(v ...string) string {
	for _, x := range v {
		if strings.TrimSpace(x) != "" {
			return x
		}
	}
	return ""
}

/*************** Templates ***************/
func expandTemplate(name string) []string {
	switch strings.ToLower(strings.TrimSpace(name)) {
	case "full_site_health":
		return []string{"ping", "http", "dns"}
	case "network_deep":
		return []string{"ping", "tcp", "trace", "dns"}
	case "quick":
		return []string{"http"}
	default:
		return nil
	}
}

/*************** Client IP helper ***************/
func clientIP(r *http.Request) (netip.Addr, bool) {
	if xff := strings.TrimSpace(r.Header.Get("X-Forwarded-For")); xff != "" {
		parts := strings.Split(xff, ",")
		for _, p := range parts {
			if a, err := netip.ParseAddr(strings.TrimSpace(p)); err == nil && a.IsValid() {
				return a, true
			}
		}
	}
	if xr := strings.TrimSpace(r.Header.Get("X-Real-IP")); xr != "" {
		if a, err := netip.ParseAddr(xr); err == nil && a.IsValid() {
			return a, true
		}
	}
	host, _, _ := net.SplitHostPort(r.RemoteAddr)
	if a, err := netip.ParseAddr(host); err == nil && a.IsValid() {
		return a, true
	}
	return netip.Addr{}, false
}

/*************** CRUD: Sites ***************/
func (a *App) handleCreateSite(w http.ResponseWriter, r *http.Request) {
	var req createSiteReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "bad json", http.StatusBadRequest)
		return
	}
	if strings.TrimSpace(req.Name) == "" {
		http.Error(w, "name required", http.StatusBadRequest)
		return
	}
	if (req.URL == nil || *req.URL == "") && (req.Host == nil || *req.Host == "") {
		http.Error(w, "either url or host required", http.StatusBadRequest)
		return
	}
	if req.URL != nil && *req.URL != "" && !isHTTPURL(*req.URL) {
		http.Error(w, "bad url", http.StatusBadRequest)
		return
	}
	if req.Host != nil && *req.Host != "" && !hostRe.MatchString(*req.Host) {
		http.Error(w, "bad host", http.StatusBadRequest)
		return
	}
	dnsAddr, err := parseDNSServer(req.DNSServer)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}
	kinds := sanitizeKinds(req.CheckTypes)

	var id uuid.UUID
	if err := a.pool.QueryRow(r.Context(),
		`insert into sites(name,url,host,port,dns_server,check_types) values ($1,$2,$3,$4,$5,$6) returning id`,
		req.Name, req.URL, req.Host, req.Port, dnsAddr, kinds).Scan(&id); err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}
	_ = json.NewEncoder(w).Encode(Site{ID: id, Name: req.Name, CheckTypes: kinds})
}

func (a *App) handleListSites(w http.ResponseWriter, r *http.Request) {
	rows, err := a.pool.Query(r.Context(), `select id,name,url,host,port,dns_server,check_types,created_at from sites order by created_at desc`)
	if err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}
	defer rows.Close()

	var items []Site
	for rows.Next() {
		var s Site
		var urlN, hostN, dnsN sql.NullString
		var portN sql.NullInt32
		if err := rows.Scan(&s.ID, &s.Name, &urlN, &hostN, &portN, &dnsN, &s.CheckTypes, &s.CreatedAt); err != nil {
			http.Error(w, "db error", http.StatusInternalServerError)
			return
		}
		if urlN.Valid {
			u := urlN.String
			s.URL = &u
		}
		if hostN.Valid {
			h := hostN.String
			s.Host = &h
		}
		if portN.Valid {
			p := int(portN.Int32)
			s.Port = &p
		}
		if dnsN.Valid && dnsN.String != "" {
			if a1, err := netip.ParseAddr(dnsN.String); err == nil {
				s.DNSServer = &a1
			}
		}
		items = append(items, s)
	}
	_ = json.NewEncoder(w).Encode(listResp[Site]{Items: items})
}

func (a *App) handleGetSite(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	if _, err := uuid.Parse(id); err != nil {
		http.Error(w, "bad id", http.StatusBadRequest)
		return
	}
	var s Site
	var urlN, hostN, dnsN sql.NullString
	var portN sql.NullInt32
	if err := a.pool.QueryRow(r.Context(),
		`select id,name,url,host,port,dns_server,check_types,created_at from sites where id=$1`, id).
		Scan(&s.ID, &s.Name, &urlN, &hostN, &portN, &dnsN, &s.CheckTypes, &s.CreatedAt); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			http.Error(w, "not found", http.StatusNotFound)
			return
		}
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}
	if urlN.Valid {
		u := urlN.String
		s.URL = &u
	}
	if hostN.Valid {
		h := hostN.String
		s.Host = &h
	}
	if portN.Valid {
		p := int(portN.Int32)
		s.Port = &p
	}
	if dnsN.Valid && dnsN.String != "" {
		if a1, err := netip.ParseAddr(dnsN.String); err == nil {
			s.DNSServer = &a1
		}
	}
	_ = json.NewEncoder(w).Encode(s)
}

func (a *App) handleDeleteSite(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	if _, err := uuid.Parse(id); err != nil {
		http.Error(w, "bad id", http.StatusBadRequest)
		return
	}
	cmd, err := a.pool.Exec(r.Context(), `delete from sites where id=$1`, id)
	if err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}
	if cmd.RowsAffected() == 0 {
		http.Error(w, "not found", http.StatusNotFound)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

func (a *App) handleListServices(w http.ResponseWriter, r *http.Request) {
	ids := make([]string, 0, len(popularServices))
	for _, svc := range popularServices {
		ids = append(ids, svc.ID)
	}

	stats, err := a.fetchReviewStats(r.Context(), ids)
	if err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}

	items := make([]serviceListItem, 0, len(popularServices))
	for _, svc := range popularServices {
		st := stats[svc.ID]
		avg, count := aggregateRatings(&svc, st)
		items = append(items, serviceListItem{
			ID:           svc.ID,
			Name:         svc.Name,
			URL:          svc.URL,
			Status:       svc.Status,
			Rating:       round1(avg),
			Reviews:      count,
			HourlyChecks: append([]int(nil), svc.HourlyChecks...),
		})
	}

	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(listResp[serviceListItem]{Items: items})
}

func (a *App) handleListServiceReviews(w http.ResponseWriter, r *http.Request) {
	serviceID := chi.URLParam(r, "id")
	svc, ok := serviceIndex[serviceID]
	if !ok {
		http.Error(w, "service not found", http.StatusNotFound)
		return
	}

	rows, err := a.pool.Query(r.Context(), `
                select id, rating, review, created_at
                from service_reviews
                where service_id=$1
                order by created_at desc
                limit 200
        `, serviceID)
	if err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}
	defer rows.Close()

	reviews := make([]serviceReviewItem, 0)
	for rows.Next() {
		var item serviceReviewItem
		if err := rows.Scan(&item.ID, &item.Rating, &item.Text, &item.CreatedAt); err != nil {
			http.Error(w, "db error", http.StatusInternalServerError)
			return
		}
		reviews = append(reviews, item)
	}
	if err := rows.Err(); err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}

	stats, err := a.fetchReviewStats(r.Context(), []string{serviceID})
	if err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}
	st := stats[serviceID]
	avg, count := aggregateRatings(svc, st)

	resp := serviceReviewsResp{
		Reviews:       reviews,
		ReviewCount:   count,
		AverageRating: round1(avg),
	}

	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(resp)
}

func (a *App) handleCreateServiceReview(w http.ResponseWriter, r *http.Request) {
	serviceID := chi.URLParam(r, "id")
	svc, ok := serviceIndex[serviceID]
	if !ok {
		http.Error(w, "service not found", http.StatusNotFound)
		return
	}

	userHeader := strings.TrimSpace(r.Header.Get("X-User-Id"))
	if userHeader == "" {
		http.Error(w, "unauthorized", http.StatusUnauthorized)
		return
	}
	userID, err := uuid.Parse(userHeader)
	if err != nil {
		http.Error(w, "unauthorized", http.StatusUnauthorized)
		return
	}

	var req createServiceReviewReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "bad json", http.StatusBadRequest)
		return
	}
	if req.Rating < 1 || req.Rating > 5 {
		http.Error(w, "rating must be 1..5", http.StatusBadRequest)
		return
	}
	req.Text = strings.TrimSpace(req.Text)
	if req.Text == "" {
		http.Error(w, "text required", http.StatusBadRequest)
		return
	}
	if len([]rune(req.Text)) > 4000 {
		http.Error(w, "text too long", http.StatusBadRequest)
		return
	}

	var reviewID uuid.UUID
	var createdAt time.Time
	if err := a.pool.QueryRow(r.Context(),
		`insert into service_reviews(service_id,user_id,rating,review) values($1,$2,$3,$4) returning id,created_at`,
		serviceID, userID, req.Rating, req.Text,
	).Scan(&reviewID, &createdAt); err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}

	stats, err := a.fetchReviewStats(r.Context(), []string{serviceID})
	if err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}
	st := stats[serviceID]
	avg, count := aggregateRatings(svc, st)

	resp := createServiceReviewResp{
		Review: serviceReviewItem{
			ID:        reviewID,
			Rating:    req.Rating,
			Text:      req.Text,
			CreatedAt: createdAt,
		},
		ReviewCount:   count,
		AverageRating: round1(avg),
	}

	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusCreated)
	_ = json.NewEncoder(w).Encode(resp)
}

/*************** Кэш результатов + постановка задач ***************/
type cachedValue struct {
	CheckID string          `json:"check_id"`
	Kind    string          `json:"kind"`
	Status  string          `json:"status"`  // ok|cancelled|fail
	Payload json.RawMessage `json:"payload"` // []ReportObservation
	Metrics json.RawMessage `json:"metrics"`
	TS      string          `json:"ts"`
}

func (a *App) handleCreateChecks(w http.ResponseWriter, r *http.Request) {
	siteID := chi.URLParam(r, "id")
	if _, err := uuid.Parse(siteID); err != nil {
		http.Error(w, "bad site id", http.StatusBadRequest)
		return
	}

	// читаем сайт
	var s Site
	var urlN, hostN, dnsN sql.NullString
	var portN sql.NullInt32
	if err := a.pool.QueryRow(r.Context(),
		`select id,name,url,host,port,dns_server,check_types,created_at from sites where id=$1`, siteID).
		Scan(&s.ID, &s.Name, &urlN, &hostN, &portN, &dnsN, &s.CheckTypes, &s.CreatedAt); err != nil {
		http.Error(w, "site not found", http.StatusNotFound)
		return
	}
	if urlN.Valid {
		u := urlN.String
		s.URL = &u
	}
	if hostN.Valid {
		h := hostN.String
		s.Host = &h
	}
	if portN.Valid {
		p := int(portN.Int32)
		s.Port = &p
	}
	if dnsN.Valid && dnsN.String != "" {
		if a1, err := netip.ParseAddr(dnsN.String); err == nil {
			s.DNSServer = &a1
		}
	}

	// тело запроса опционально
	var req createChecksReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil && !errors.Is(err, io.EOF) {
		http.Error(w, "bad json", http.StatusBadRequest)
		return
	}

	// виды: приоритет Types > Template > site.check_types
	kinds := sanitizeKinds(req.Types)
	if len(kinds) == 0 && strings.TrimSpace(req.Template) != "" {
		kinds = sanitizeKinds(expandTemplate(req.Template))
	}
	if len(kinds) == 0 {
		kinds = s.CheckTypes
	}
	if len(kinds) == 0 {
		http.Error(w, "no kinds to run", http.StatusBadRequest)
		return
	}

	// инициатор и IP (из заголовков)
	var createdBy *uuid.UUID
	if v := strings.TrimSpace(r.Header.Get("X-User-Id")); v != "" {
		if uid, err := uuid.Parse(v); err == nil {
			createdBy = &uid
		}
	}
	var reqIP *string
	if aaddr, ok := clientIP(r); ok {
		ipStr := aaddr.String()
		reqIP = &ipStr
	}

	// 1) пробуем кэш по каждому виду
	cached := make(map[string]cachedValue)
	missing := make([]string, 0, len(kinds))
	for _, k := range kinds {
		key := fmt.Sprintf("recent:%s:%s", s.ID.String(), k)
		raw, err := a.redis.Get(r.Context(), key).Result()
		if err == nil && raw != "" {
			var cv cachedValue
			if json.Unmarshal([]byte(raw), &cv) == nil {
				cached[k] = cv
				continue
			}
		}
		missing = append(missing, k)
	}

	// 2) создаём check (status зависит от наличия кэша/пропусков) + created_by/request_ip
	status := "queued"
	if len(missing) == 0 {
		status = "done"
	} else if len(cached) > 0 {
		status = "running"
	}
	var checkID uuid.UUID
	var err error
	switch {
	case createdBy != nil && reqIP != nil:
		err = a.pool.QueryRow(r.Context(),
			`insert into checks(site_id,status,created_by,request_ip) values($1,$2,$3,$4) returning id`,
			s.ID, status, *createdBy, *reqIP).Scan(&checkID)
	case createdBy != nil:
		err = a.pool.QueryRow(r.Context(),
			`insert into checks(site_id,status,created_by) values($1,$2,$3) returning id`,
			s.ID, status, *createdBy).Scan(&checkID)
	case reqIP != nil:
		err = a.pool.QueryRow(r.Context(),
			`insert into checks(site_id,status,request_ip) values($1,$2,$3) returning id`,
			s.ID, status, *reqIP).Scan(&checkID)
	default:
		err = a.pool.QueryRow(r.Context(),
			`insert into checks(site_id,status) values($1,$2) returning id`,
			s.ID, status).Scan(&checkID)
	}
	if err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}

	// 3) если был кэш — сразу запишем его в check_results и опубликуем SSE
	if len(cached) > 0 {
		for k, cv := range cached {
			if _, err := a.pool.Exec(r.Context(),
				`insert into check_results(check_id, kind, status, payload, metrics, stream_id)
				 values ($1,$2,$3,$4,$5,NULL)`,
				checkID, k, cv.Status, cv.Payload, cv.Metrics); err != nil {
				a.log.Warn("cache_to_db_fail", "kind", k, "err", err)
			}
			// publish "result" (source=cache)
			_ = a.publishCheckUpdate(r.Context(), checkID, "result", map[string]any{
				"check_id": checkID, "kind": k, "status": cv.Status, "payload": json.RawMessage(cv.Payload), "metrics": json.RawMessage(cv.Metrics), "source": "cache",
			})
		}
		_, _ = a.pool.Exec(r.Context(), `update checks set started_at = coalesce(started_at, now()) where id=$1`, checkID)
	}

	// 4) отсутствующие виды — построим spec и XADD
	queued := make([]string, 0, len(missing))
	skipped := make([]string, 0)
	for _, kind := range missing {
		spec, berr := a.buildSpec(kind, s, req.Args[kind])
		if berr != nil {
			skipped = append(skipped, fmt.Sprintf("%s:error:%v", kind, berr))
			continue
		}

		// дедуп за короткое окно
		sum := sha1.Sum(spec)
		dupKey := fmt.Sprintf("last:site:%s:%s:%s", s.ID, kind, hex.EncodeToString(sum[:]))
		ok, rerr := a.redis.SetNX(r.Context(), dupKey, "1", a.cfg.TaskTTL).Result()
		if rerr != nil {
			skipped = append(skipped, fmt.Sprintf("%s:redis_err", kind))
			continue
		}
		if !ok {
			skipped = append(skipped, fmt.Sprintf("%s:dup", kind))
			continue
		}

		taskID := uuid.New()
		val := map[string]interface{}{"task_id": taskID.String(), "check_id": checkID.String(), "kind": kind, "spec": string(spec)}
		if id, err := a.redis.XAdd(r.Context(), &redis.XAddArgs{Stream: a.cfg.StreamTasks, Values: val}).Result(); err != nil {
			skipped = append(skipped, fmt.Sprintf("%s:xadd_err", kind))
			continue
		} else {
			a.observeTaskStreamWrite(r.Context(), a.cfg.StreamTasks, id, kind)
		}
		queued = append(queued, kind)
	}

	// publish статус постановки
	_ = a.publishCheckUpdate(r.Context(), checkID, "status", map[string]any{
		"check_id": checkID, "queued": queued, "cached": mapKeys(cached), "skipped": skipped,
	})

	resp := createChecksResp{CheckID: checkID, Queued: queued, Cached: mapKeys(cached), Skipped: skipped}
	_ = json.NewEncoder(w).Encode(resp)
}

// buildSpec формирует дискриминированный union для агента.
// Если rawOverride != nil — вливаем его, но гарантируем kind=<kind>.
func (a *App) buildSpec(kind string, s Site, rawOverride json.RawMessage) (json.RawMessage, error) {
	if len(rawOverride) > 0 && strings.TrimSpace(string(rawOverride)) != "" {
		var m map[string]any
		if err := json.Unmarshal(rawOverride, &m); err != nil {
			return nil, fmt.Errorf("bad override json: %w", err)
		}
		m["kind"] = kind
		return json.Marshal(m)
	}
	switch kind {
	case "http":
		if s.URL == nil || *s.URL == "" {
			return nil, fmt.Errorf("site.url required for http")
		}
		return json.Marshal(map[string]any{"kind": "http", "url": *s.URL})
	case "ping":
		host := firstNonEmpty(ptrStr(s.Host), hostFromSiteURL(s))
		if host == "" {
			return nil, fmt.Errorf("host required for ping")
		}
		return json.Marshal(map[string]any{"kind": "ping", "host": host, "count": 3})
	case "tcp":
		host := firstNonEmpty(ptrStr(s.Host), hostFromSiteURL(s))
		if host == "" {
			return nil, fmt.Errorf("host required for tcp")
		}
		port := 0
		if s.Port != nil && *s.Port > 0 {
			port = *s.Port
		} else if s.URL != nil {
			_, port = urlHost(*s.URL)
		}
		if port == 0 {
			port = 80
		}
		return json.Marshal(map[string]any{"kind": "tcp", "host": host, "port": port})
	case "dns":
		domain := firstNonEmpty(ptrStr(s.Host), hostFromSiteURL(s))
		if domain == "" {
			return nil, fmt.Errorf("host/domain required for dns")
		}
		out := map[string]any{"kind": "dns", "query": fmt.Sprintf("A %s", domain)}
		if s.DNSServer != nil {
			out["server"] = s.DNSServer.String()
		}
		return json.Marshal(out)
	case "trace":
		host := firstNonEmpty(ptrStr(s.Host), hostFromSiteURL(s))
		if host == "" {
			return nil, fmt.Errorf("host required for trace")
		}
		return json.Marshal(map[string]any{"kind": "trace", "host": host, "max_hops": 30})
	default:
		return nil, fmt.Errorf("unsupported kind %q", kind)
	}
}

/*************** Views ***************/
func (a *App) handleGetCheck(w http.ResponseWriter, r *http.Request) {
	checkID := chi.URLParam(r, "check_id")
	if _, err := uuid.Parse(checkID); err != nil {
		http.Error(w, "bad check id", http.StatusBadRequest)
		return
	}
	var id, siteID uuid.UUID
	var status string
	var created time.Time
	var started, finished sql.NullTime
	if err := a.pool.QueryRow(r.Context(),
		`select id, site_id, status, created_at, started_at, finished_at from checks where id=$1`, checkID).
		Scan(&id, &siteID, &status, &created, &started, &finished); err != nil {
		http.Error(w, "not found", http.StatusNotFound)
		return
	}
	rows, err := a.pool.Query(r.Context(),
		`select id, kind, status, payload, metrics, stream_id, created_at
		 from check_results where check_id=$1 order by created_at asc`, checkID)
	if err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}
	defer rows.Close()

	var results []checkResult
	for rows.Next() {
		var cr checkResult
		if err := rows.Scan(&cr.ID, &cr.Kind, &cr.Status, &cr.Payload, &cr.Metrics, &cr.StreamID, &cr.CreatedAt); err != nil {
			http.Error(w, "db error", http.StatusInternalServerError)
			return
		}
		results = append(results, cr)
	}
	view := checkView{ID: id, SiteID: siteID, Status: status, CreatedAt: created, Results: results}
	if started.Valid {
		view.StartedAt = &started.Time
	}
	if finished.Valid {
		view.FinishedAt = &finished.Time
	}
	_ = json.NewEncoder(w).Encode(view)
}

/*************** SSE: /v1/checks/{id}/stream ***************/
func (a *App) handleStreamCheck(w http.ResponseWriter, r *http.Request) {
	checkID := chi.URLParam(r, "check_id")
	if _, err := uuid.Parse(checkID); err != nil {
		http.Error(w, "bad check id", http.StatusBadRequest)
		return
	}

	// заголовки для SSE
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
	channel := fmt.Sprintf("check-upd:%s", checkID)

	// отправим initial snapshot
	var snap any
	{
		var id, siteID uuid.UUID
		var status string
		var created time.Time
		var started, finished sql.NullTime
		err := a.pool.QueryRow(ctx,
			`select id, site_id, status, created_at, started_at, finished_at from checks where id=$1`, checkID).
			Scan(&id, &siteID, &status, &created, &started, &finished)
		if err == nil {
			snap = map[string]any{"check_id": id, "site_id": siteID, "status": status, "created_at": created}
			writeSSE(w, "snapshot", snap)
			flusher.Flush()
		}
	}

	// подписка на Redis Pub/Sub
	pubsub := a.redis.Subscribe(ctx, channel)
	defer pubsub.Close()
	ch := pubsub.Channel()

	// пинги для поддержания соединения
	tick := time.NewTicker(15 * time.Second)
	defer tick.Stop()

	for {
		select {
		case <-ctx.Done():
			return
		case <-tick.C:
			// комментарий-пинг (совместимо с любыми SSE-клиентами)
			if _, err := io.WriteString(w, ": ping\n\n"); err != nil {
				return
			}
			flusher.Flush()
		case msg, ok := <-ch:
			if !ok {
				return
			}
			// данные уже JSON, просто оборачиваем в data
			writeSSERaw(w, "update", msg.Payload)
			flusher.Flush()
		}
	}
}

func writeSSE(w http.ResponseWriter, event string, v any) {
	b, _ := json.Marshal(v)
	fmt.Fprintf(w, "event: %s\n", event)
	fmt.Fprintf(w, "data: %s\n\n", string(b))
}
func writeSSERaw(w http.ResponseWriter, event string, raw string) {
	fmt.Fprintf(w, "event: %s\n", event)
	fmt.Fprintf(w, "data: %s\n\n", raw)
}

/*************** Pub/Sub helpers ***************/
func (a *App) publishCheckUpdate(ctx context.Context, checkID uuid.UUID, typ string, data any) error {
	// заворачиваем в конверт: {type, data, ts}
	env := map[string]any{
		"type": typ,
		"ts":   time.Now().UTC().Format(time.RFC3339Nano),
		"data": data,
	}
	b, _ := json.Marshal(env)
	channel := fmt.Sprintf("check-upd:%s", checkID.String())
	return a.redis.Publish(ctx, channel, string(b)).Err()
}

/*************** Monitoring ***************/
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

func (a *App) observeTaskStreamWrite(ctx context.Context, stream, id, kind string) {
	length, err := a.redis.XLen(ctx, stream).Result()
	if err != nil {
		a.log.Warn("stream_write_metrics", "stream", stream, "source", "enqueue", "kind", kind, "err", err)
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
	a.log.Info("stream_write", "stream", stream, "source", "enqueue", "kind", kind, "id", id, "stream_length", length, "lag_ms", lagMs)
}

/*************** misc ***************/
func mapKeys(m map[string]cachedValue) []string {
	out := make([]string, 0, len(m))
	for k := range m {
		out = append(out, k)
	}
	return out
}
