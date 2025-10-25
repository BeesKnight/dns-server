package main

import (
	"context"
	"crypto/sha1"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/netip"
	"net/url"
	"regexp"
	"sort"
	"strconv"
	"strings"

	"github.com/google/uuid"
	"github.com/jackc/pgx/v5"
	"github.com/redis/go-redis/v9"
)

type quickCheckReq struct {
	URL       string   `json:"url"`
	Template  *string  `json:"template,omitempty"`
	Kinds     []string `json:"kinds,omitempty"`
	DNSServer *string  `json:"dns_server,omitempty"`
	Args      any      `json:"args,omitempty"` // ignore but keep backward compatibility
}

type quickCheckResp struct {
	CheckID uuid.UUID `json:"check_id"`
	Queued  []string  `json:"queued,omitempty"`
	Cached  []string  `json:"cached,omitempty"`
	Skipped []string  `json:"skipped,omitempty"`
}

type quickCachedValue struct {
	Status  string          `json:"status"`
	Payload json.RawMessage `json:"payload"`
	Metrics json.RawMessage `json:"metrics"`
	TS      string          `json:"ts"`
}

type quickCheckMeta struct {
	CacheKey string
}

var (
	hostRe         = regexp.MustCompile(`^[a-zA-Z0-9\.\-]+$`)
	errRateLimited = errors.New("rate limited")
)

func (a *App) handleQuickCheckCreate(w http.ResponseWriter, r *http.Request) {
	var req quickCheckReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "bad json", http.StatusBadRequest)
		return
	}
	rawURL := strings.TrimSpace(req.URL)
	if rawURL == "" || !isHTTPURL(rawURL) {
		http.Error(w, "bad url", http.StatusBadRequest)
		return
	}
	parsed, err := url.Parse(rawURL)
	if err != nil {
		http.Error(w, "bad url", http.StatusBadRequest)
		return
	}
	if parsed.Host == "" {
		http.Error(w, "url host required", http.StatusBadRequest)
		return
	}
	normalizedURL := parsed.String()

	templateName := ""
	if req.Template != nil {
		templateName = strings.ToLower(strings.TrimSpace(*req.Template))
	}
	if templateName == "" {
		templateName = "quick"
	}
	templateKinds := expandTemplate(templateName)
	if templateKinds == nil {
		http.Error(w, "unknown template", http.StatusBadRequest)
		return
	}

	kinds := sanitizeKinds(req.Kinds)
	tk := sanitizeKinds(templateKinds)
	kinds = mergeKinds(kinds, tk)
	if len(kinds) == 0 {
		http.Error(w, "no kinds to run", http.StatusBadRequest)
		return
	}

	dnsAddr, err := parseDNSServer(req.DNSServer)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}

	reqIP := strings.TrimSpace(parseIP(r))
	var createdBy *uuid.UUID
	if v := strings.TrimSpace(r.Header.Get("X-User-Id")); v != "" {
		if uid, err := uuid.Parse(v); err == nil {
			createdBy = &uid
		}
	}

	if err := a.enforceQuickRateLimit(r.Context(), createdBy, reqIP); err != nil {
		if errors.Is(err, errRateLimited) {
			http.Error(w, "too many quick checks", http.StatusTooManyRequests)
			return
		}
		a.log.Warn("quick_rate_limit_error", "err", err)
		http.Error(w, "rate limit error", http.StatusInternalServerError)
		return
	}

	dedupSeed := strings.ToLower(normalizedURL) + "|" + templateName + "|" + strings.Join(kinds, ",")
	if dnsAddr != nil {
		dedupSeed += "|" + dnsAddr.String()
	}
	cachePrefix := "quick:" + sha256Hex(dedupSeed)

	cached := make(map[string]quickCachedValue)
	missing := make([]string, 0, len(kinds))
	for _, kind := range kinds {
		key := fmt.Sprintf("recent:%s:%s", cachePrefix, kind)
		raw, err := a.redis.Get(r.Context(), key).Result()
		if err == nil && raw != "" {
			var cv quickCachedValue
			if json.Unmarshal([]byte(raw), &cv) == nil {
				cached[kind] = cv
				continue
			}
		}
		missing = append(missing, kind)
	}

	status := "queued"
	if len(missing) == 0 {
		status = "done"
	} else if len(cached) > 0 {
		status = "running"
	}

	var createdByVal any
	if createdBy != nil {
		createdByVal = *createdBy
	}
	var reqIPVal any
	if reqIP != "" {
		reqIPVal = reqIP
	}

	var checkID uuid.UUID
	if err := a.pool.QueryRow(r.Context(),
		`insert into checks(site_id,status,created_by,request_ip) values ($1,$2,$3,$4) returning id`,
		nil, status, createdByVal, reqIPVal).Scan(&checkID); err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}

	var dnsVal any
	if dnsAddr != nil {
		dnsVal = dnsAddr.String()
	}
	templateVal := sqlNull(templateName)
	if _, err := a.pool.Exec(r.Context(),
		`insert into quick_checks(check_id,url,template,kinds,dns_server,cache_key) values ($1,$2,$3,$4,$5,$6)`,
		checkID, normalizedURL, templateVal, kinds, dnsVal, cachePrefix); err != nil {
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}

	if len(cached) > 0 {
		for kind, cv := range cached {
			if _, err := a.pool.Exec(r.Context(),
				`insert into check_results(check_id, kind, status, payload, metrics, stream_id) values ($1,$2,$3,$4,$5,NULL)`,
				checkID, kind, cv.Status, cv.Payload, cv.Metrics); err != nil {
				a.log.Warn("quick_cache_to_db_fail", "kind", kind, "err", err)
			}
			_ = a.publishUpdate(r.Context(), checkID, "result", map[string]any{
				"check_id": checkID,
				"kind":     kind,
				"status":   cv.Status,
				"payload":  json.RawMessage(cv.Payload),
				"metrics":  json.RawMessage(cv.Metrics),
				"source":   "cache",
			})
		}
		_, _ = a.pool.Exec(r.Context(), `update checks set started_at = coalesce(started_at, now()) where id=$1`, checkID)
		if len(missing) == 0 {
			_, _ = a.pool.Exec(r.Context(), `update checks set finished_at = coalesce(finished_at, now()) where id=$1`, checkID)
			_ = a.publishUpdate(r.Context(), checkID, "done", map[string]any{"check_id": checkID, "source": "cache"})
		}
	}

	queued := make([]string, 0, len(missing))
	skipped := make([]string, 0)
	info := quickTargetInfo{URL: normalizedURL, DNSServer: dnsAddr}
	for _, kind := range missing {
		spec, berr := buildQuickSpec(kind, info)
		if berr != nil {
			skipped = append(skipped, fmt.Sprintf("%s:error:%v", kind, berr))
			continue
		}
		sum := sha1.Sum(spec)
		dupKey := fmt.Sprintf("last:%s:%s", cachePrefix, hex.EncodeToString(sum[:]))
		ok, rerr := a.redis.SetNX(r.Context(), dupKey, "1", a.cfg.QuickDedupTTL).Result()
		if rerr != nil {
			skipped = append(skipped, fmt.Sprintf("%s:redis_err", kind))
			continue
		}
		if !ok {
			skipped = append(skipped, fmt.Sprintf("%s:dup", kind))
			continue
		}
		taskID := uuid.New()
		val := map[string]any{"task_id": taskID.String(), "check_id": checkID.String(), "kind": kind, "spec": string(spec)}
		if id, err := a.redis.XAdd(r.Context(), &redis.XAddArgs{Stream: a.cfg.StreamTasks, Values: val}).Result(); err != nil {
			skipped = append(skipped, fmt.Sprintf("%s:xadd_err", kind))
			continue
		} else {
			a.observeStreamWrite(r.Context(), a.cfg.StreamTasks, id, fmt.Sprintf("quick:%s", kind))
		}
		queued = append(queued, kind)
	}

	startData := map[string]any{
		"check_id": checkID,
		"url":      normalizedURL,
		"template": templateName,
		"kinds":    kinds,
		"queued":   queued,
		"cached":   mapKeys(cached),
		"skipped":  skipped,
	}
	if createdBy != nil {
		startData["created_by"] = createdBy.String()
	}
	if reqIP != "" {
		startData["request_ip"] = reqIP
	}
	_ = a.publishUpdate(r.Context(), checkID, "check.start", startData)
	_ = a.publishUpdate(r.Context(), checkID, "status", map[string]any{
		"check_id": checkID,
		"queued":   queued,
		"cached":   mapKeys(cached),
		"skipped":  skipped,
	})

	resp := quickCheckResp{CheckID: checkID, Queued: queued, Cached: mapKeys(cached), Skipped: skipped}
	_ = json.NewEncoder(w).Encode(resp)
}

func (a *App) enforceQuickRateLimit(ctx context.Context, userID *uuid.UUID, ip string) error {
	keys := []struct {
		key   string
		limit int
	}{}
	if userID != nil && a.cfg.QuickRatePerUser > 0 {
		keys = append(keys, struct {
			key   string
			limit int
		}{key: fmt.Sprintf("ratelimit:quick:user:%s", userID.String()), limit: a.cfg.QuickRatePerUser})
	}
	if strings.TrimSpace(ip) != "" && a.cfg.QuickRatePerIP > 0 {
		keys = append(keys, struct {
			key   string
			limit int
		}{key: fmt.Sprintf("ratelimit:quick:ip:%s", ip), limit: a.cfg.QuickRatePerIP})
	}
	for _, entry := range keys {
		count, err := a.redis.Incr(ctx, entry.key).Result()
		if err != nil {
			return err
		}
		if count == 1 {
			if err := a.redis.Expire(ctx, entry.key, a.cfg.QuickRateWindow).Err(); err != nil {
				a.log.Warn("quick_rate_expire_failed", "key", entry.key, "err", err)
			}
		}
		if int(count) > entry.limit {
			return errRateLimited
		}
	}
	return nil
}

func (a *App) quickCacheMeta(ctx context.Context, checkID uuid.UUID) (*quickCheckMeta, error) {
	var cacheKey string
	if err := a.pool.QueryRow(ctx, `select cache_key from quick_checks where check_id=$1`, checkID).Scan(&cacheKey); err != nil {
		if errors.Is(err, pgx.ErrNoRows) {
			return nil, nil
		}
		return nil, err
	}
	return &quickCheckMeta{CacheKey: cacheKey}, nil
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

func mergeKinds(existing, extra []string) []string {
	if len(existing) == 0 {
		existing = make([]string, 0, len(extra))
	}
	seen := map[string]struct{}{}
	for _, k := range existing {
		seen[k] = struct{}{}
	}
	for _, k := range extra {
		if _, ok := seen[k]; !ok {
			existing = append(existing, k)
			seen[k] = struct{}{}
		}
	}
	sort.Strings(existing)
	return existing
}

func isHTTPURL(u string) bool {
	pu, err := url.Parse(u)
	return err == nil && (pu.Scheme == "http" || pu.Scheme == "https") && pu.Host != ""
}

func parseDNSServer(raw *string) (*netip.Addr, error) {
	if raw == nil || *raw == "" {
		return nil, nil
	}
	addr, err := netip.ParseAddr(strings.TrimSpace(*raw))
	if err != nil {
		return nil, fmt.Errorf("bad dns_server: %w", err)
	}
	return &addr, nil
}

func sqlNull(v string) any {
	if strings.TrimSpace(v) == "" {
		return nil
	}
	return v
}

type quickTargetInfo struct {
	URL       string
	DNSServer *netip.Addr
}

func buildQuickSpec(kind string, info quickTargetInfo) (json.RawMessage, error) {
	pu, err := url.Parse(info.URL)
	if err != nil {
		return nil, fmt.Errorf("bad url")
	}
	host := pu.Hostname()
	if host == "" {
		return nil, fmt.Errorf("host required")
	}
	switch kind {
	case "http":
		return json.Marshal(map[string]any{"kind": "http", "url": info.URL})
	case "ping":
		return json.Marshal(map[string]any{"kind": "ping", "host": host, "count": 3})
	case "tcp":
		portStr := pu.Port()
		port := 0
		if portStr != "" {
			if p, err := strconv.Atoi(portStr); err == nil {
				port = p
			}
		}
		if port == 0 {
			switch strings.ToLower(pu.Scheme) {
			case "https":
				port = 443
			default:
				port = 80
			}
		}
		return json.Marshal(map[string]any{"kind": "tcp", "host": host, "port": port})
	case "dns":
		query := host
		if !hostRe.MatchString(query) {
			return nil, fmt.Errorf("host/domain required for dns")
		}
		payload := map[string]any{"kind": "dns", "query": fmt.Sprintf("A %s", query)}
		if info.DNSServer != nil {
			payload["server"] = info.DNSServer.String()
		}
		return json.Marshal(payload)
	case "trace":
		return json.Marshal(map[string]any{"kind": "trace", "host": host, "max_hops": 30})
	default:
		return nil, fmt.Errorf("unsupported kind %q", kind)
	}
}

func mapKeys[K comparable, V any](m map[K]V) []K {
	out := make([]K, 0, len(m))
	for k := range m {
		out = append(out, k)
	}
	sort.Slice(out, func(i, j int) bool { return fmt.Sprint(out[i]) < fmt.Sprint(out[j]) })
	return out
}
