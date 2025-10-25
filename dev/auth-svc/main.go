package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"net/netip"
	"os"
	"os/signal"
	"regexp"
	"strings"
	"syscall"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/go-chi/chi/v5/middleware"
	"github.com/golang-jwt/jwt/v5"
	"github.com/google/uuid"
	"github.com/jackc/pgx/v5/pgxpool"
	"golang.org/x/crypto/bcrypt"
)

/*************** Config ***************/
type Config struct {
	HTTPAddr    string
	DatabaseURL string

	JWTSecret string
	Audience  string
	Issuer    string

	AccessTTL time.Duration

	AllowRegister bool
}

func env(k, def string) string {
	if v := strings.TrimSpace(os.Getenv(k)); v != "" {
		return v
	}
	return def
}
func benv(k string, def bool) bool {
	v := strings.ToLower(strings.TrimSpace(os.Getenv(k)))
	if v == "" {
		return def
	}
	return v == "1" || v == "true" || v == "yes"
}
func denv(k, def string) time.Duration {
	if v := strings.TrimSpace(os.Getenv(k)); v != "" {
		if d, err := time.ParseDuration(v); err == nil {
			return d
		}
	}
	d, _ := time.ParseDuration(def)
	return d
}

func loadConfig() Config {
	return Config{
		HTTPAddr:      env("HTTP_ADDR", ":8080"),
		DatabaseURL:   env("DATABASE_URL", "postgres://postgres:dev@postgres:5432/aezacheck?sslmode=disable"),
		JWTSecret:     env("JWT_SECRET", "dev_secret_change_me"),
		Audience:      env("JWT_AUDIENCE", "aezacheck"),
		Issuer:        env("JWT_ISSUER", "auth-svc"),
		AccessTTL:     denv("ACCESS_TTL", "1h"),
		AllowRegister: benv("ALLOW_REGISTER", true),
	}
}

/*************** App ***************/
type App struct {
	cfg  Config
	log  *slog.Logger
	pool *pgxpool.Pool
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
	r.Use(middleware.RequestID, middleware.RealIP, middleware.Logger, middleware.Recoverer)

	// health
	r.Get("/livez", func(w http.ResponseWriter, _ *http.Request) { w.WriteHeader(http.StatusOK) })
	r.Get("/readyz", func(w http.ResponseWriter, r *http.Request) {
		if err := app.pool.Ping(r.Context()); err != nil {
			http.Error(w, "db not ready", http.StatusServiceUnavailable)
			return
		}
		w.WriteHeader(http.StatusOK)
	})

	// public
	r.Route("/v1/auth", func(rt chi.Router) {
		rt.Post("/login", app.handleLogin)
		if app.cfg.AllowRegister {
			rt.Post("/register", app.handleRegister)
		}
		// protected
		rt.Group(func(pr chi.Router) {
			pr.Use(app.jwtAuth())
			pr.Get("/me", app.handleMe)

			// user IPs management
			pr.Get("/ips", app.handleIPsList)
			pr.Patch("/ips/{id}", app.handleIPUpdate)
			pr.Delete("/ips/{id}", app.handleIPDelete)
		})
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
	return &App{cfg: cfg, log: log, pool: pool}, nil
}

func (a *App) Close() {
	if a.pool != nil {
		a.pool.Close()
	}
}

/*************** DB migrate ***************/
func migrate(ctx context.Context, db *pgxpool.Pool) error {
	stmts := []string{
		`create extension if not exists "pgcrypto";`,
		`create table if not exists users (
			id uuid primary key default gen_random_uuid(),
			email text unique not null,
			pass_hash text not null,
			role text not null default 'user',
			created_at timestamptz not null default now(),
			last_login_at timestamptz
		);`,
		`create table if not exists user_ips (
			id uuid primary key default gen_random_uuid(),
			user_id uuid not null references users(id) on delete cascade,
			ip inet not null,
			label text,
			is_active boolean not null default true,
			first_seen timestamptz not null default now(),
			last_seen timestamptz not null default now(),
			unique(user_id, ip)
		);`,
		`create index if not exists idx_user_ips_user on user_ips(user_id);`,
	}
	for _, s := range stmts {
		if _, err := db.Exec(ctx, s); err != nil {
			return err
		}
	}
	return nil
}

/*************** Models / DTOs ***************/
type userRow struct {
	ID        uuid.UUID
	Email     string
	Role      string
	CreatedAt time.Time
	LastLogin sql.NullTime
}
type loginReq struct {
	Email    string `json:"email"`
	Password string `json:"password"`
}
type registerReq struct {
	Email    string `json:"email"`
	Password string `json:"password"`
}
type loginResp struct {
	AccessToken string `json:"access_token"`
	ExpiresIn   int64  `json:"expires_in"`
	User        any    `json:"user"`
}

/*************** Helpers ***************/
var emailRE = regexp.MustCompile(`^[^\s@]+@[^\s@]+\.[^\s@]+$`)

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

/*************** JWT helpers ***************/
type userClaims struct {
	UserID uuid.UUID `json:"uid"`
	Email  string    `json:"email"`
	Role   string    `json:"role"`
	jwt.RegisteredClaims
}

func (a *App) signAccessToken(uid uuid.UUID, email, role string) (string, time.Time, error) {
	exp := time.Now().Add(a.cfg.AccessTTL)
	now := time.Now()
	claims := userClaims{
		UserID: uid,
		Email:  email,
		Role:   role,
		RegisteredClaims: jwt.RegisteredClaims{
			Subject:   uid.String(),
			Audience:  jwt.ClaimStrings{a.cfg.Audience},
			Issuer:    a.cfg.Issuer,
			IssuedAt:  jwt.NewNumericDate(now),
			NotBefore: jwt.NewNumericDate(now),
			ExpiresAt: jwt.NewNumericDate(exp),
		},
	}
	t := jwt.NewWithClaims(jwt.SigningMethodHS256, claims)
	signed, err := t.SignedString([]byte(a.cfg.JWTSecret))
	return signed, exp, err
}
func (a *App) parseAccessToken(raw string) (*userClaims, error) {
	claims := &userClaims{}
	_, err := jwt.ParseWithClaims(raw, claims, func(t *jwt.Token) (interface{}, error) {
		if t.Method != jwt.SigningMethodHS256 {
			return nil, fmt.Errorf("unexpected alg")
		}
		return []byte(a.cfg.JWTSecret), nil
	},
		jwt.WithAudience(a.cfg.Audience),
		jwt.WithIssuer(a.cfg.Issuer),
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

/*************** Auth middlewares ***************/
type ctxUserKey struct{}
type authUser struct {
	ID    uuid.UUID
	Email string
	Role  string
}

func (a *App) jwtAuth() func(http.Handler) http.Handler {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			raw := bearer(r)
			if raw == "" {
				http.Error(w, "missing bearer", http.StatusUnauthorized)
				return
			}
			claims, err := a.parseAccessToken(raw)
			if err != nil {
				http.Error(w, "invalid token", http.StatusUnauthorized)
				return
			}
			u := &authUser{ID: claims.UserID, Email: claims.Email, Role: claims.Role}
			ctx := context.WithValue(r.Context(), ctxUserKey{}, u)
			next.ServeHTTP(w, r.WithContext(ctx))
		})
	}
}
func userFromCtx(ctx context.Context) *authUser {
	v, _ := ctx.Value(ctxUserKey{}).(*authUser)
	return v
}

/*************** Handlers ***************/
func (a *App) handleRegister(w http.ResponseWriter, r *http.Request) {
	var req registerReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "bad json", http.StatusBadRequest)
		return
	}
	req.Email = strings.ToLower(strings.TrimSpace(req.Email))
	if !emailRE.MatchString(req.Email) || len(req.Password) < 6 {
		http.Error(w, "bad email/password", http.StatusBadRequest)
		return
	}
	hash, _ := bcrypt.GenerateFromPassword([]byte(req.Password), bcrypt.DefaultCost)

	var id uuid.UUID
	err := a.pool.QueryRow(r.Context(),
		`insert into users(email, pass_hash) values ($1,$2) returning id`, req.Email, string(hash)).Scan(&id)
	if err != nil {
		http.Error(w, "email exists or db error", http.StatusConflict)
		return
	}

	// токен
	token, exp, _ := a.signAccessToken(id, req.Email, "user")

	// ip лог
	if ip, ok := clientIP(r); ok {
		_, _ = a.pool.Exec(r.Context(),
			`insert into user_ips(user_id, ip) values ($1,$2)
			 on conflict(user_id,ip) do update set last_seen=now()`, id, ip.String())
	}
	_, _ = a.pool.Exec(r.Context(), `update users set last_login_at=now() where id=$1`, id)

	resp := loginResp{
		AccessToken: token,
		ExpiresIn:   int64(time.Until(exp).Seconds()),
		User:        map[string]any{"id": id, "email": req.Email, "role": "user"},
	}
	_ = json.NewEncoder(w).Encode(resp)
}

func (a *App) handleLogin(w http.ResponseWriter, r *http.Request) {
	var req loginReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "bad json", http.StatusBadRequest)
		return
	}
	email := strings.ToLower(strings.TrimSpace(req.Email))
	if !emailRE.MatchString(email) || strings.TrimSpace(req.Password) == "" {
		http.Error(w, "bad email/password", http.StatusBadRequest)
		return
	}
	// найти пользователя
	var u userRow
	var passHash string
	err := a.pool.QueryRow(r.Context(),
		`select id, email, role, created_at, last_login_at, pass_hash from users where email=$1`, email).
		Scan(&u.ID, &u.Email, &u.Role, &u.CreatedAt, &u.LastLogin, &passHash)
	if err != nil {
		http.Error(w, "invalid credentials", http.StatusUnauthorized)
		return
	}
	if bcrypt.CompareHashAndPassword([]byte(passHash), []byte(req.Password)) != nil {
		http.Error(w, "invalid credentials", http.StatusUnauthorized)
		return
	}
	token, exp, _ := a.signAccessToken(u.ID, u.Email, u.Role)

	// записать IP
	if ip, ok := clientIP(r); ok {
		_, _ = a.pool.Exec(r.Context(),
			`insert into user_ips(user_id, ip) values ($1,$2)
			 on conflict(user_id,ip) do update set last_seen=now()`, u.ID, ip.String())
	}
	_, _ = a.pool.Exec(r.Context(), `update users set last_login_at=now() where id=$1`, u.ID)

	resp := loginResp{
		AccessToken: token,
		ExpiresIn:   int64(time.Until(exp).Seconds()),
		User:        map[string]any{"id": u.ID, "email": u.Email, "role": u.Role},
	}
	_ = json.NewEncoder(w).Encode(resp)
}

func (a *App) handleMe(w http.ResponseWriter, r *http.Request) {
	u := userFromCtx(r.Context())
	if u == nil {
		http.Error(w, "unauthorized", http.StatusUnauthorized)
		return
	}

	var lastLogin sql.NullTime
	_ = a.pool.QueryRow(r.Context(), `select last_login_at from users where id=$1`, u.ID).Scan(&lastLogin)

	out := map[string]any{
		"id": u.ID, "email": u.Email, "role": u.Role,
		"last_login_at": nil,
	}
	if lastLogin.Valid {
		out["last_login_at"] = lastLogin.Time
	}

	// обновим ip-активность (полезно для “онлайна”)
	if ip, ok := clientIP(r); ok {
		_, _ = a.pool.Exec(r.Context(),
			`insert into user_ips(user_id, ip) values ($1,$2)
			 on conflict(user_id,ip) do update set last_seen=now()`, u.ID, ip.String())
		out["current_ip"] = ip.String()
	}
	_ = json.NewEncoder(w).Encode(out)
}

/*************** IPs management ***************/
type ipItem struct {
	ID        uuid.UUID `json:"id"`
	IP        string    `json:"ip"`
	Label     string    `json:"label,omitempty"`
	IsActive  bool      `json:"is_active"`
	FirstSeen time.Time `json:"first_seen"`
	LastSeen  time.Time `json:"last_seen"`
}

func (a *App) handleIPsList(w http.ResponseWriter, r *http.Request) {
	u := userFromCtx(r.Context())
	if u == nil {
		http.Error(w, "unauthorized", http.StatusUnauthorized)
		return
	}

	rows, err := a.pool.Query(r.Context(),
		`select id, ip::text, coalesce(label,''), is_active, first_seen, last_seen
		   from user_ips where user_id=$1 order by last_seen desc limit 50`, u.ID)
	if err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}
	defer rows.Close()

	items := make([]ipItem, 0, 16)
	for rows.Next() {
		var it ipItem
		if err := rows.Scan(&it.ID, &it.IP, &it.Label, &it.IsActive, &it.FirstSeen, &it.LastSeen); err == nil {
			items = append(items, it)
		}
	}
	_ = json.NewEncoder(w).Encode(map[string]any{"items": items})
}

type ipUpdateReq struct {
	Label    *string `json:"label,omitempty"`
	IsActive *bool   `json:"is_active,omitempty"`
}

func (a *App) handleIPUpdate(w http.ResponseWriter, r *http.Request) {
	u := userFromCtx(r.Context())
	if u == nil {
		http.Error(w, "unauthorized", http.StatusUnauthorized)
		return
	}

	idStr := chi.URLParam(r, "id")
	ipid, err := uuid.Parse(idStr)
	if err != nil {
		http.Error(w, "bad id", http.StatusBadRequest)
		return
	}

	var req ipUpdateReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "bad json", http.StatusBadRequest)
		return
	}

	// проверим принадлежность записи пользователю
	var cnt int
	_ = a.pool.QueryRow(r.Context(), `select count(1) from user_ips where id=$1 and user_id=$2`, ipid, u.ID).Scan(&cnt)
	if cnt == 0 {
		http.Error(w, "not found", http.StatusNotFound)
		return
	}

	if req.Label != nil {
		_, _ = a.pool.Exec(r.Context(), `update user_ips set label=$1 where id=$2`, strings.TrimSpace(*req.Label), ipid)
	}
	if req.IsActive != nil {
		_, _ = a.pool.Exec(r.Context(), `update user_ips set is_active=$1 where id=$2`, *req.IsActive, ipid)
	}
	w.WriteHeader(http.StatusNoContent)
}

func (a *App) handleIPDelete(w http.ResponseWriter, r *http.Request) {
	u := userFromCtx(r.Context())
	if u == nil {
		http.Error(w, "unauthorized", http.StatusUnauthorized)
		return
	}

	idStr := chi.URLParam(r, "id")
	ipid, err := uuid.Parse(idStr)
	if err != nil {
		http.Error(w, "bad id", http.StatusBadRequest)
		return
	}

	_, err = a.pool.Exec(r.Context(), `delete from user_ips where id=$1 and user_id=$2`, ipid, u.ID)
	if err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}
