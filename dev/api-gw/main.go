package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"net/http/httputil"
	"net/netip"
	"net/url"
	"os"
	"os/signal"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/go-chi/chi/v5/middleware"
	"github.com/golang-jwt/jwt/v5"
	"golang.org/x/time/rate"
)

/*************** Config ***************/
type Config struct {
	HTTPAddr string

	AuthBase  string // http://auth-svc:8080
	SitesBase string // http://sites-svc:8081
	JobsBase  string // http://jobs-svc:8082

	JWTSecret   string
	JWTAudience string
	JWTIssuer   string

	// CORS
	AllowedOrigins string // "*" или "http://localhost:3000,https://app.example.com"

	// Rate limit
	RateEnabled    bool
	RateRPS        float64
	RateBurst      int
	RateSkipAgents bool // не лимитить /v1/agents/**

	// Basic → JWT bridge
	BasicToJWTEnabled bool
	LoginTimeoutSec   int
}

func env(k, def string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return def
}
func benv(k string, def bool) bool {
	v := strings.TrimSpace(strings.ToLower(os.Getenv(k)))
	if v == "" {
		return def
	}
	return v == "1" || v == "true" || v == "yes"
}
func fenv(k string, def float64) float64 {
	if v := strings.TrimSpace(os.Getenv(k)); v != "" {
		var f float64
		if _, err := fmt.Sscan(v, &f); err == nil {
			return f
		}
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

func loadConfig() Config {
	return Config{
		HTTPAddr: env("HTTP_ADDR", ":8088"),

		AuthBase:  env("AUTH_BASE_URL", "http://auth-svc:8080"),
		SitesBase: env("SITES_BASE_URL", "http://sites-svc:8081"),
		JobsBase:  env("JOBS_BASE_URL", "http://jobs-svc:8082"),

		JWTSecret:   env("JWT_SECRET", "dev_secret_change_me"),
		JWTAudience: env("JWT_AUDIENCE", "aezacheck"),
		JWTIssuer:   env("JWT_ISSUER", "auth-svc"),

		AllowedOrigins: env("CORS_ALLOWED_ORIGINS", "*"),

		RateEnabled:    benv("RATE_ENABLED", true),
		RateRPS:        fenv("RATE_RPS", 5.0),
		RateBurst:      ienv("RATE_BURST", 10),
		RateSkipAgents: benv("RATE_SKIP_AGENTS", true),

		BasicToJWTEnabled: benv("BASIC_TO_JWT_ENABLED", true),
		LoginTimeoutSec:   ienv("LOGIN_TIMEOUT_SEC", 5),
	}
}

/*************** App ***************/
type App struct {
	cfg     Config
	log     *slog.Logger
	authRP  *httputil.ReverseProxy
	sitesRP *httputil.ReverseProxy
	jobsRP  *httputil.ReverseProxy
	rl      *rateTable

	httpClient *http.Client
	tcache     *tokenCache
}

func main() {
	cfg := loadConfig()
	logger := slog.New(slog.NewTextHandler(os.Stdout, &slog.HandlerOptions{Level: slog.LevelInfo}))
	slog.SetDefault(logger)

	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()

	app, err := initApp(cfg, logger)
	if err != nil {
		logger.Error("init", "err", err)
		os.Exit(1)
	}

	r := chi.NewRouter()
	r.Use(middleware.RequestID, middleware.RealIP, middleware.Recoverer, middleware.Logger)

	// CORS
	r.Use(app.corsMiddleware())

	// Rate-limit
	if app.cfg.RateEnabled {
		r.Use(app.rateLimitMiddleware())
	}

	// health
	r.Get("/livez", func(w http.ResponseWriter, _ *http.Request) { w.WriteHeader(http.StatusOK) })
	r.Get("/readyz", func(w http.ResponseWriter, _ *http.Request) { w.WriteHeader(http.StatusOK) })

	// Public: auth
	r.Mount("/v1/auth", app.proxyMount(app.authRP))

	// Agents: напрямую в jobs-svc (аутентификация в jobs-svc по X-Agent-*)
	r.Mount("/v1/agents", app.proxyMount(app.jobsRP))

	// Protected: JWT (или Basic → JWT), затем инжект идентичности
	r.Group(func(pr chi.Router) {
		if app.cfg.BasicToJWTEnabled {
			pr.Use(app.basicToJWT())
		}
		pr.Use(app.jwtAuth())
		pr.Use(app.injectIdentityHeaders())

		// === Карта и Geo ===
		pr.Mount("/v1/map", app.proxyMount(app.jobsRP))
		pr.Mount("/v1/geo", app.proxyMount(app.jobsRP))

		// === Тонкий спец-роут: география чека ===
		// Отдаём логичный URL /v1/checks/{id}/geo, но обслуживает jobs-svc.
		// Размещаем ДО общего монтирования /v1/checks → sites-svc.
		pr.Handle("/v1/checks/{id}/geo", app.proxyHandler(app.jobsRP))
		// для совместимости с {check_id}
		pr.Handle("/v1/checks/{check_id}/geo", app.proxyHandler(app.jobsRP))

		// === Основные ресурсы checks/sites → sites-svc ===
		pr.Mount("/v1/sites", app.proxyMount(app.sitesRP))
		pr.Mount("/v1/checks", app.proxyMount(app.sitesRP))
		pr.Handle("/v1/jobs/checks", app.proxyHandler(app.jobsRP))
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

func initApp(cfg Config, log *slog.Logger) (*App, error) {
	authRP, err := newProxy(cfg.AuthBase)
	if err != nil {
		return nil, fmt.Errorf("auth proxy: %w", err)
	}
	sitesRP, err := newProxy(cfg.SitesBase)
	if err != nil {
		return nil, fmt.Errorf("sites proxy: %w", err)
	}
	jobsRP, err := newProxy(cfg.JobsBase)
	if err != nil {
		return nil, fmt.Errorf("jobs proxy: %w", err)
	}

	app := &App{
		cfg:        cfg,
		log:        log,
		authRP:     authRP,
		sitesRP:    sitesRP,
		jobsRP:     jobsRP,
		rl:         newRateTable(time.Minute * 10),
		httpClient: &http.Client{Timeout: time.Duration(cfg.LoginTimeoutSec) * time.Second},
		tcache:     newTokenCache(),
	}
	return app, nil
}

/*************** Reverse proxy helpers ***************/
func newProxy(base string) (*httputil.ReverseProxy, error) {
	u, err := url.Parse(base)
	if err != nil {
		return nil, err
	}
	rp := httputil.NewSingleHostReverseProxy(u)
	rp.Director = func(req *http.Request) {
		originalHost := req.Host
		req.URL.Scheme = u.Scheme
		req.URL.Host = u.Host
		req.Host = u.Host
		if originalHost != "" {
			req.Header.Set("X-Forwarded-Host", originalHost)
		}
		remoteIP := strings.TrimSpace(req.RemoteAddr)
		if host, _, err := net.SplitHostPort(req.RemoteAddr); err == nil {
			remoteIP = host
		}
		if remoteIP != "" {
			xff := strings.TrimSpace(req.Header.Get("X-Forwarded-For"))
			if xff != "" {
				parts := strings.Split(xff, ",")
				last := strings.TrimSpace(parts[len(parts)-1])
				if last != remoteIP {
					xff = xff + ", " + remoteIP
				}
			} else {
				xff = remoteIP
			}
			req.Header.Set("X-Forwarded-For", xff)
			if strings.TrimSpace(req.Header.Get("X-Real-IP")) == "" {
				client := strings.TrimSpace(strings.Split(xff, ",")[0])
				if client != "" {
					req.Header.Set("X-Real-IP", client)
				}
			}
		}
		if xf := req.Header.Get("X-Forwarded-Proto"); xf == "" {
			if req.TLS != nil {
				req.Header.Set("X-Forwarded-Proto", "https")
			} else {
				req.Header.Set("X-Forwarded-Proto", "http")
			}
		}
	}
	rp.ErrorHandler = func(w http.ResponseWriter, r *http.Request, e error) {
		http.Error(w, "upstream error", http.StatusBadGateway)
	}
	return rp, nil
}

func (a *App) proxyMount(rp *httputil.ReverseProxy) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		rp.ServeHTTP(w, r)
	})
}

// Используем для отдельных маршрутов (например, /v1/checks/{id}/geo → jobs-svc)
func (a *App) proxyHandler(rp *httputil.ReverseProxy) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		rp.ServeHTTP(w, r)
	})
}

/*************** JWT ***************/
type ctxUserKey struct{}
type authUser struct {
	ID    string
	Email string
	Role  string
}
type userClaims struct {
	UserID string `json:"uid"`
	Email  string `json:"email"`
	Role   string `json:"role"`
	jwt.RegisteredClaims
}

func (a *App) parseAccessToken(raw string) (*userClaims, error) {
	claims := &userClaims{}
	_, err := jwt.ParseWithClaims(raw, claims, func(t *jwt.Token) (interface{}, error) {
		if t.Method != jwt.SigningMethodHS256 {
			return nil, fmt.Errorf("unexpected alg")
		}
		return []byte(a.cfg.JWTSecret), nil
	},
		jwt.WithAudience(a.cfg.JWTAudience),
		jwt.WithIssuer(a.cfg.JWTIssuer),
	)
	if err != nil {
		return nil, err
	}
	return claims, nil
}
func bearer(r *http.Request) string {
	h := strings.TrimSpace(r.Header.Get("Authorization"))
	if h == "" {
		return ""
	}
	if !strings.HasPrefix(strings.ToLower(h), "bearer ") {
		return ""
	}
	return strings.TrimSpace(h[7:])
}

func (a *App) jwtAuth() func(http.Handler) http.Handler {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			raw := bearer(r)
			if raw == "" {
				if tok := strings.TrimSpace(r.URL.Query().Get("access_token")); tok != "" {
					raw = tok
				}
			}
			if raw == "" {
				http.Error(w, "missing bearer", http.StatusUnauthorized)
				return
			}
			claims, err := a.parseAccessToken(raw)
			if err != nil {
				http.Error(w, "invalid token", http.StatusUnauthorized)
				return
			}
			user := &authUser{ID: claims.UserID, Email: claims.Email, Role: claims.Role}
			ctx := context.WithValue(r.Context(), ctxUserKey{}, user)
			next.ServeHTTP(w, r.WithContext(ctx))
		})
	}
}

func userFromCtx(ctx context.Context) *authUser {
	v, _ := ctx.Value(ctxUserKey{}).(*authUser)
	return v
}

/*************** Basic → JWT middleware ***************/
type loginResp struct {
	AccessToken string `json:"access_token"`
	ExpiresIn   int64  `json:"expires_in"`
}

func (a *App) basicToJWT() func(http.Handler) http.Handler {
	loginURL := strings.TrimRight(a.cfg.AuthBase, "/") + "/v1/auth/login"

	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			auth := strings.TrimSpace(r.Header.Get("Authorization"))
			if auth == "" || !strings.HasPrefix(strings.ToLower(auth), "basic ") {
				next.ServeHTTP(w, r)
				return
			}
			raw := strings.TrimSpace(auth[len("Basic "):])
			decoded, err := base64.StdEncoding.DecodeString(raw)
			if err != nil {
				http.Error(w, "invalid basic auth", http.StatusUnauthorized)
				return
			}
			creds := string(decoded)
			colon := strings.IndexByte(creds, ':')
			if colon <= 0 {
				http.Error(w, "invalid basic auth", http.StatusUnauthorized)
				return
			}
			email := strings.TrimSpace(creds[:colon])
			pass := creds[colon+1:]

			cacheKey := sha256Hex(email + "\x00" + pass)
			if tok, ok := a.tcache.Get(cacheKey); ok {
				r.Header.Set("Authorization", "Bearer "+tok)
				next.ServeHTTP(w, r)
				return
			}

			reqBody, _ := json.Marshal(map[string]string{"email": email, "password": pass})
			req, err := http.NewRequestWithContext(r.Context(), http.MethodPost, loginURL, bytes.NewReader(reqBody))
			if err != nil {
				http.Error(w, "auth upstream error", http.StatusBadGateway)
				return
			}
			req.Header.Set("Content-Type", "application/json")

			resp, err := a.httpClient.Do(req)
			if err != nil {
				http.Error(w, "auth upstream error", http.StatusBadGateway)
				return
			}
			defer resp.Body.Close()

			if resp.StatusCode != http.StatusOK {
				http.Error(w, "invalid credentials", http.StatusUnauthorized)
				return
			}

			var lr loginResp
			if err := json.NewDecoder(resp.Body).Decode(&lr); err != nil || strings.TrimSpace(lr.AccessToken) == "" {
				http.Error(w, "auth parse error", http.StatusBadGateway)
				return
			}

			ttl := time.Duration(lr.ExpiresIn) * time.Second
			if ttl <= 0 {
				ttl = time.Minute
			}
			ttl = ttl - 10*time.Second
			if ttl < 15*time.Second {
				ttl = 15 * time.Second
			}
			a.tcache.Set(cacheKey, lr.AccessToken, ttl)

			r.Header.Set("Authorization", "Bearer "+lr.AccessToken)
			next.ServeHTTP(w, r)
		})
	}
}

/*************** Identity header injection ***************/
func (a *App) injectIdentityHeaders() func(http.Handler) http.Handler {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			if u := userFromCtx(r.Context()); u != nil && strings.TrimSpace(u.ID) != "" {
				r.Header.Set("X-User-Id", u.ID)
			}
			if ip, ok := clientIP(r); ok {
				r.Header.Set("X-Client-IP", ip.String())
				r.Header.Set("X-Real-IP", ip.String())
			}
			next.ServeHTTP(w, r)
		})
	}
}

/*************** CORS ***************/
func (a *App) corsMiddleware() func(http.Handler) http.Handler {
	allowed := strings.TrimSpace(a.cfg.AllowedOrigins)
	allowAny := allowed == "*" || allowed == ""

	var list []string
	if !allowAny {
		for _, p := range strings.Split(allowed, ",") {
			if s := strings.TrimSpace(p); s != "" {
				list = append(list, s)
			}
		}
	}
	isAllowed := func(origin string) bool {
		if allowAny {
			return true
		}
		for _, o := range list {
			if strings.EqualFold(origin, o) {
				return true
			}
		}
		return false
	}

	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			origin := r.Header.Get("Origin")
			if origin != "" && isAllowed(origin) {
				if allowAny {
					w.Header().Set("Access-Control-Allow-Origin", "*")
				} else {
					w.Header().Set("Access-Control-Allow-Origin", origin)
					w.Header().Set("Vary", "Origin")
					w.Header().Set("Access-Control-Allow-Credentials", "true")
				}
				w.Header().Set("Access-Control-Allow-Methods", "GET,POST,PUT,PATCH,DELETE,OPTIONS")
				w.Header().Set("Access-Control-Allow-Headers", "Authorization, Content-Type, X-Requested-With, X-User-Id")
				w.Header().Set("Access-Control-Expose-Headers", "Content-Type, X-Request-Id")
				w.Header().Set("Access-Control-Max-Age", "600")
			}
			if r.Method == http.MethodOptions {
				w.WriteHeader(http.StatusNoContent)
				return
			}
			next.ServeHTTP(w, r)
		})
	}
}

/*************** Rate limit ***************/
type rateItem struct {
	l        *rate.Limiter
	lastSeen time.Time
}
type rateTable struct {
	mu       sync.Mutex
	items    map[string]*rateItem
	lifetime time.Duration
}

func newRateTable(lifetime time.Duration) *rateTable {
	rt := &rateTable{
		items:    make(map[string]*rateItem),
		lifetime: lifetime,
	}
	go func() {
		t := time.NewTicker(time.Minute)
		defer t.Stop()
		for range t.C {
			rt.cleanup()
		}
	}()
	return rt
}
func (rt *rateTable) get(key string, rps float64, burst int) *rate.Limiter {
	now := time.Now()
	rt.mu.Lock()
	defer rt.mu.Unlock()
	if it, ok := rt.items[key]; ok {
		it.lastSeen = now
		return it.l
	}
	lim := rate.NewLimiter(rate.Limit(rps), burst)
	rt.items[key] = &rateItem{l: lim, lastSeen: now}
	return lim
}
func (rt *rateTable) cleanup() {
	now := time.Now()
	rt.mu.Lock()
	defer rt.mu.Unlock()
	for k, it := range rt.items {
		if now.Sub(it.lastSeen) > rt.lifetime {
			delete(rt.items, k)
		}
	}
}

func (a *App) rateLimitMiddleware() func(http.Handler) http.Handler {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			if a.cfg.RateSkipAgents && strings.HasPrefix(r.URL.Path, "/v1/agents") {
				next.ServeHTTP(w, r)
				return
			}
			key := "unknown"
			if ip, ok := clientIP(r); ok {
				key = ip.String()
			}
			lim := a.rl.get(key, a.cfg.RateRPS, a.cfg.RateBurst)
			if !lim.Allow() {
				w.Header().Set("Retry-After", "1")
				http.Error(w, "rate limit exceeded", http.StatusTooManyRequests)
				return
			}
			next.ServeHTTP(w, r)
		})
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

/*************** Token cache (in-memory) ***************/
type tokenCacheItem struct {
	token string
	exp   time.Time
}
type tokenCache struct {
	mu sync.Mutex
	m  map[string]tokenCacheItem
}

func newTokenCache() *tokenCache {
	tc := &tokenCache{m: make(map[string]tokenCacheItem)}
	go func() {
		t := time.NewTicker(30 * time.Second)
		defer t.Stop()
		for range t.C {
			tc.gc()
		}
	}()
	return tc
}
func (tc *tokenCache) Get(key string) (string, bool) {
	now := time.Now()
	tc.mu.Lock()
	defer tc.mu.Unlock()
	if it, ok := tc.m[key]; ok {
		if now.Before(it.exp) {
			return it.token, true
		}
		delete(tc.m, key)
	}
	return "", false
}
func (tc *tokenCache) Set(key, tok string, ttl time.Duration) {
	tc.mu.Lock()
	defer tc.mu.Unlock()
	tc.m[key] = tokenCacheItem{token: tok, exp: time.Now().Add(ttl)}
}
func (tc *tokenCache) gc() {
	now := time.Now()
	tc.mu.Lock()
	defer tc.mu.Unlock()
	for k, it := range tc.m {
		if now.After(it.exp) {
			delete(tc.m, k)
		}
	}
}

/*************** utils ***************/
func sha256Hex(s string) string {
	h := sha256.Sum256([]byte(s))
	return hex.EncodeToString(h[:])
}
