package main

import (
	"bufio"
	"bytes"
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	miniredis "github.com/alicebob/miniredis/v2"
	"github.com/go-chi/chi/v5"
	"github.com/google/uuid"
	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgconn"
	"github.com/oschwald/geoip2-golang"
	"github.com/redis/go-redis/v9"
)

func TestRegisterRespJSONShape(t *testing.T) {
	resp := registerResp{
		AgentID:            7,
		AuthToken:          "token",
		LeaseDurationMs:    1500,
		HeartbeatTimeoutMs: 5000,
	}
	raw, err := json.Marshal(resp)
	if err != nil {
		t.Fatalf("marshal registerResp: %v", err)
	}
	var payload map[string]any
	if err := json.Unmarshal(raw, &payload); err != nil {
		t.Fatalf("unmarshal payload: %v", err)
	}
	for _, key := range []string{"agent_id", "auth_token", "lease_duration_ms", "heartbeat_timeout_ms"} {
		if _, ok := payload[key]; !ok {
			t.Fatalf("expected key %q in register response", key)
		}
	}
}

func TestClaimRespJSONShape(t *testing.T) {
	resp := claimResp{Leases: []leaseDTO{{LeaseID: 1, TaskID: 2, Kind: "dns", LeaseUntilMs: 123, Spec: json.RawMessage(`{"kind":"dns"}`)}}}
	raw, err := json.Marshal(resp)
	if err != nil {
		t.Fatalf("marshal claimResp: %v", err)
	}
	var payload map[string]any
	if err := json.Unmarshal(raw, &payload); err != nil {
		t.Fatalf("unmarshal payload: %v", err)
	}
	leases, ok := payload["leases"].([]any)
	if !ok || len(leases) != 1 {
		t.Fatalf("expected leases array with one entry, got %#v", payload["leases"])
	}
	lease := leases[0].(map[string]any)
	for _, key := range []string{"lease_id", "task_id", "kind", "lease_until_ms", "spec"} {
		if _, ok := lease[key]; !ok {
			t.Fatalf("expected key %q in lease payload", key)
		}
	}
}

func TestHandleClaimDrainsBacklog(t *testing.T) {
	mr, err := miniredis.Run()
	if err != nil {
		t.Fatalf("start miniredis: %v", err)
	}
	defer mr.Close()

	ctx := context.Background()
	rdb := redis.NewClient(&redis.Options{Addr: mr.Addr(), Protocol: 2})
	defer rdb.Close()

	stream := "check_tasks"
	taskID := uuid.New()
	checkID := uuid.New()
	spec := `{"query":"example.com"}`
	msgID, err := rdb.XAdd(ctx, &redis.XAddArgs{
		Stream: stream,
		Values: map[string]any{
			"task_id":  taskID.String(),
			"check_id": checkID.String(),
			"kind":     "dns",
			"spec":     spec,
		},
	}).Result()
	if err != nil {
		t.Fatalf("xadd backlog: %v", err)
	}

	leaseID := uuid.New()
	leaseExternalID := int64(99)
	leaseDeadline := time.Now().Add(90 * time.Second).UTC()

	fp := &fakePool{}
	fp.queryRowFn = func(_ context.Context, query string, args ...any) pgx.Row {
		switch {
		case strings.Contains(query, "select count(1) from leases"):
			return fakeRow{values: []any{int64(0)}}
		case strings.Contains(query, "insert into leases"):
			if len(args) < 7 {
				return fakeRow{err: fmt.Errorf("unexpected args for insert: %d", len(args))}
			}
			if streamID, ok := args[5].(string); !ok || streamID != msgID {
				return fakeRow{err: fmt.Errorf("unexpected stream id: %#v", args[5])}
			}
			return fakeRow{values: []any{leaseID, leaseExternalID, leaseDeadline}}
		case strings.Contains(query, "select site_id::text, request_ip::text from checks where id=$1"):
			return fakeRow{values: []any{sql.NullString{}, sql.NullString{}}}
		default:
			return fakeRow{err: fmt.Errorf("unexpected query: %s", query)}
		}
	}

	app := &App{
		cfg: Config{
			StreamTasks:     stream,
			ClaimBlockMs:    50,
			LeaseTTL:        90 * time.Second,
			MapPubChannel:   "map:events",
			MapStream:       "map_events",
			MapStreamMaxLen: 100,
		},
		log:   slog.New(slog.NewTextHandler(io.Discard, nil)),
		pool:  fp,
		redis: rdb,
	}

	ag := &agentAuth{ID: uuid.New(), ExternalID: 123, Limit: 1}
	reqBody := strings.NewReader(fmt.Sprintf(`{"agent_id":%d,"capacities":{"dns":1}}`, ag.ExternalID))
	req := httptest.NewRequest(http.MethodPost, "/v1/agents/claim", reqBody)
	req = req.WithContext(context.WithValue(req.Context(), agentAuthKey{}, ag))

	w := httptest.NewRecorder()
	app.handleClaim(w, req)

	if w.Code != http.StatusOK {
		t.Fatalf("expected 200 OK, got %d: %s", w.Code, w.Body.String())
	}

	var resp claimResp
	if err := json.Unmarshal(w.Body.Bytes(), &resp); err != nil {
		t.Fatalf("unmarshal claim resp: %v", err)
	}
	if len(resp.Leases) != 1 {
		t.Fatalf("expected one lease, got %d", len(resp.Leases))
	}
	lease := resp.Leases[0]
	if lease.LeaseID != uint64(leaseExternalID) {
		t.Fatalf("unexpected lease id %d", lease.LeaseID)
	}
	if lease.TaskID != uuidToUint64(taskID) {
		t.Fatalf("unexpected task id %d", lease.TaskID)
	}

	if ttl := mr.TTL(leaseKey(msgID)); ttl <= 0 {
		t.Fatalf("expected lease lock to be set, ttl=%v", ttl)
	}
}

func TestExtendRespJSONShape(t *testing.T) {
	resp := extendResp{Outcomes: []extendOutcome{{LeaseID: 5, NewDeadlineMs: 12345}}}
	raw, err := json.Marshal(resp)
	if err != nil {
		t.Fatalf("marshal extendResp: %v", err)
	}
	var payload map[string]any
	if err := json.Unmarshal(raw, &payload); err != nil {
		t.Fatalf("unmarshal payload: %v", err)
	}
	outcomes, ok := payload["outcomes"].([]any)
	if !ok || len(outcomes) != 1 {
		t.Fatalf("expected outcomes array with one entry, got %#v", payload["outcomes"])
	}
	entry := outcomes[0].(map[string]any)
	for _, key := range []string{"lease_id", "new_deadline_ms"} {
		if _, ok := entry[key]; !ok {
			t.Fatalf("expected key %q in extend outcome", key)
		}
	}
}

func TestReportRespJSONShape(t *testing.T) {
	resp := reportResp{Acknowledged: 3}
	raw, err := json.Marshal(resp)
	if err != nil {
		t.Fatalf("marshal reportResp: %v", err)
	}
	var payload map[string]any
	if err := json.Unmarshal(raw, &payload); err != nil {
		t.Fatalf("unmarshal payload: %v", err)
	}
	if val, ok := payload["acknowledged"].(float64); !ok || val != 3 {
		t.Fatalf("expected acknowledged count 3, got %#v", payload["acknowledged"])
	}
}

func TestObservationsPayloadNormalizes(t *testing.T) {
	unit := "ms"
	payload := observationsPayload([]reportObservation{{Name: "latency", Value: nil, Unit: &unit}})
	var decoded map[string]any
	if err := json.Unmarshal(payload, &decoded); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	obs, ok := decoded["observations"].([]any)
	if !ok || len(obs) != 1 {
		t.Fatalf("expected single observation, got %#v", decoded["observations"])
	}
	entry := obs[0].(map[string]any)
	if entry["value"] != nil {
		t.Fatalf("expected null value, got %#v", entry["value"])
	}
	if entry["unit"] != unit {
		t.Fatalf("expected unit %q, got %#v", unit, entry["unit"])
	}
}

func TestHandleQuickCheckCreateQueuesTasks(t *testing.T) {
	mr, err := miniredis.Run()
	if err != nil {
		t.Fatalf("start miniredis: %v", err)
	}
	defer mr.Close()

	rdb := redis.NewClient(&redis.Options{Addr: mr.Addr(), Protocol: 2})
	defer rdb.Close()

	checkID := uuid.New()
	fp := &fakePool{}
	fp.queryRowFn = func(_ context.Context, sql string, args ...any) pgx.Row {
		if !strings.Contains(sql, "insert into checks") {
			return fakeRow{err: fmt.Errorf("unexpected query: %s", sql)}
		}
		return fakeRow{values: []any{checkID}}
	}
	var quickInserted bool
	fp.execFn = func(_ context.Context, sql string, args ...any) (pgconn.CommandTag, error) {
		switch {
		case strings.Contains(sql, "insert into quick_checks"):
			quickInserted = true
			return pgconn.NewCommandTag("INSERT 1"), nil
		case strings.Contains(sql, "update checks set started_at"):
			return pgconn.NewCommandTag("UPDATE 1"), nil
		case strings.Contains(sql, "update checks set finished_at"):
			return pgconn.NewCommandTag("UPDATE 1"), nil
		default:
			return pgconn.NewCommandTag(""), fmt.Errorf("unexpected exec: %s", sql)
		}
	}

	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	app := &App{
		cfg: Config{
			StreamTasks:      "check_tasks",
			PubPrefix:        "check-upd:",
			CacheTTL:         30 * time.Second,
			QuickDedupTTL:    30 * time.Second,
			QuickRateWindow:  time.Minute,
			QuickRatePerUser: 10,
			QuickRatePerIP:   10,
		},
		log:   logger,
		pool:  fp,
		redis: rdb,
	}

	ctx := context.Background()
	pubsub := rdb.Subscribe(ctx, app.cfg.PubPrefix+checkID.String())
	defer pubsub.Close()
	if _, err := pubsub.ReceiveTimeout(ctx, time.Second); err != nil {
		t.Fatalf("subscribe ack: %v", err)
	}
	msgCh := pubsub.Channel()

	body := strings.NewReader(`{"url":"https://example.com"}`)
	req := httptest.NewRequest(http.MethodPost, "/v1/jobs/checks", body)
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("X-User-Id", uuid.New().String())
	req.Header.Set("X-Forwarded-For", "203.0.113.10")

	w := httptest.NewRecorder()
	app.handleQuickCheckCreate(w, req)

	if w.Code != http.StatusOK {
		t.Fatalf("unexpected status %d: %s", w.Code, w.Body.String())
	}
	var resp quickCheckResp
	if err := json.Unmarshal(w.Body.Bytes(), &resp); err != nil {
		t.Fatalf("unmarshal resp: %v", err)
	}
	if resp.CheckID != checkID {
		t.Fatalf("unexpected check id %s", resp.CheckID)
	}
	if len(resp.Queued) != 1 || resp.Queued[0] != "http" {
		t.Fatalf("expected http queued, got %#v", resp.Queued)
	}
	if !quickInserted {
		t.Fatalf("expected quick_checks insert")
	}

	msgs, err := rdb.XRange(ctx, app.cfg.StreamTasks, "-", "+").Result()
	if err != nil {
		t.Fatalf("xrange: %v", err)
	}
	if len(msgs) != 1 {
		t.Fatalf("expected 1 queued task, got %d", len(msgs))
	}
	if msgs[0].Values["check_id"].(string) != checkID.String() {
		t.Fatalf("unexpected check_id in stream: %#v", msgs[0].Values)
	}

	select {
	case payload := <-msgCh:
		var envelope map[string]any
		if err := json.Unmarshal([]byte(payload.Payload), &envelope); err != nil {
			t.Fatalf("unmarshal envelope: %v", err)
		}
		if envelope["type"] != "check.start" {
			t.Fatalf("expected check.start event, got %#v", envelope["type"])
		}
	case <-time.After(2 * time.Second):
		t.Fatalf("expected check.start event")
	}
}

func TestHandleQuickCheckCreateRateLimit(t *testing.T) {
	mr, err := miniredis.Run()
	if err != nil {
		t.Fatalf("start miniredis: %v", err)
	}
	defer mr.Close()

	rdb := redis.NewClient(&redis.Options{Addr: mr.Addr(), Protocol: 2})
	defer rdb.Close()

	checkID := uuid.New()
	fp := &fakePool{}
	var insertCount int
	fp.queryRowFn = func(_ context.Context, sql string, args ...any) pgx.Row {
		if !strings.Contains(sql, "insert into checks") {
			return fakeRow{err: fmt.Errorf("unexpected query: %s", sql)}
		}
		insertCount++
		return fakeRow{values: []any{checkID}}
	}
	fp.execFn = func(_ context.Context, sql string, args ...any) (pgconn.CommandTag, error) {
		switch {
		case strings.Contains(sql, "insert into quick_checks"):
			return pgconn.NewCommandTag("INSERT 1"), nil
		case strings.Contains(sql, "update checks set started_at"):
			return pgconn.NewCommandTag("UPDATE 1"), nil
		case strings.Contains(sql, "update checks set finished_at"):
			return pgconn.NewCommandTag("UPDATE 1"), nil
		default:
			return pgconn.NewCommandTag(""), nil
		}
	}

	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	app := &App{
		cfg: Config{
			StreamTasks:      "check_tasks",
			PubPrefix:        "check-upd:",
			CacheTTL:         30 * time.Second,
			QuickDedupTTL:    10 * time.Second,
			QuickRateWindow:  time.Minute,
			QuickRatePerUser: 1,
			QuickRatePerIP:   5,
		},
		log:   logger,
		pool:  fp,
		redis: rdb,
	}

	body := strings.NewReader(`{"url":"https://example.org"}`)
	req := httptest.NewRequest(http.MethodPost, "/v1/jobs/checks", body)
	userID := uuid.New().String()
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("X-User-Id", userID)
	req.Header.Set("X-Real-IP", "198.51.100.7")

	w := httptest.NewRecorder()
	app.handleQuickCheckCreate(w, req)
	if w.Code != http.StatusOK {
		t.Fatalf("first request unexpected status %d", w.Code)
	}

	req2 := httptest.NewRequest(http.MethodPost, "/v1/jobs/checks", strings.NewReader(`{"url":"https://example.org"}`))
	req2.Header.Set("Content-Type", "application/json")
	req2.Header.Set("X-User-Id", userID)
	req2.Header.Set("X-Real-IP", "198.51.100.7")
	w2 := httptest.NewRecorder()
	app.handleQuickCheckCreate(w2, req2)
	if w2.Code != http.StatusTooManyRequests {
		t.Fatalf("expected rate limit 429, got %d", w2.Code)
	}
	if insertCount != 1 {
		t.Fatalf("expected single insert, got %d", insertCount)
	}
}

func TestHandleQuickCheckCreateEmitsCachedResults(t *testing.T) {
	mr, err := miniredis.Run()
	if err != nil {
		t.Fatalf("start miniredis: %v", err)
	}
	defer mr.Close()

	rdb := redis.NewClient(&redis.Options{Addr: mr.Addr(), Protocol: 2})
	defer rdb.Close()

	checkID := uuid.New()
	fp := &fakePool{}
	fp.queryRowFn = func(_ context.Context, sql string, args ...any) pgx.Row {
		switch {
		case strings.Contains(sql, "insert into checks"):
			return fakeRow{values: []any{checkID}}
		default:
			return fakeRow{err: fmt.Errorf("unexpected query: %s", sql)}
		}
	}
	var quickInserted bool
	var resultsInserted int
	fp.execFn = func(_ context.Context, sql string, args ...any) (pgconn.CommandTag, error) {
		switch {
		case strings.Contains(sql, "insert into quick_checks"):
			quickInserted = true
			return pgconn.NewCommandTag("INSERT 1"), nil
		case strings.Contains(sql, "insert into check_results"):
			resultsInserted++
			return pgconn.NewCommandTag("INSERT 1"), nil
		case strings.Contains(sql, "update checks set started_at"):
			return pgconn.NewCommandTag("UPDATE 1"), nil
		case strings.Contains(sql, "update checks set finished_at"):
			return pgconn.NewCommandTag("UPDATE 1"), nil
		default:
			return pgconn.NewCommandTag(""), fmt.Errorf("unexpected exec: %s", sql)
		}
	}

	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	app := &App{
		cfg: Config{
			StreamTasks:      "check_tasks",
			PubPrefix:        "check-upd:",
			CacheTTL:         time.Minute,
			QuickDedupTTL:    time.Minute,
			QuickRateWindow:  time.Minute,
			QuickRatePerUser: 5,
			QuickRatePerIP:   5,
		},
		log:   logger,
		pool:  fp,
		redis: rdb,
	}

	seed := strings.ToLower("https://cached.example") + "|quick|http"
	prefix := "quick:" + sha256Hex(seed)
	cacheKey := fmt.Sprintf("recent:%s:%s", prefix, "http")
	cachedVal := quickCachedValue{Status: "ok", Payload: json.RawMessage(`{"latency":10}`), Metrics: json.RawMessage(`{"age":5}`), TS: time.Now().UTC().Format(time.RFC3339Nano)}
	rawCache, _ := json.Marshal(cachedVal)
	if err := rdb.Set(context.Background(), cacheKey, string(rawCache), 0).Err(); err != nil {
		t.Fatalf("seed cache: %v", err)
	}

	ctx := context.Background()
	pubsub := rdb.Subscribe(ctx, app.cfg.PubPrefix+checkID.String())
	defer pubsub.Close()
	if _, err := pubsub.ReceiveTimeout(ctx, time.Second); err != nil {
		t.Fatalf("subscribe ack: %v", err)
	}
	msgCh := pubsub.Channel()

	body := strings.NewReader(`{"url":"https://cached.example"}`)
	req := httptest.NewRequest(http.MethodPost, "/v1/jobs/checks", body)
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("X-Forwarded-For", "192.0.2.44")

	w := httptest.NewRecorder()
	app.handleQuickCheckCreate(w, req)

	if w.Code != http.StatusOK {
		t.Fatalf("unexpected status %d: %s", w.Code, w.Body.String())
	}
	var resp quickCheckResp
	if err := json.Unmarshal(w.Body.Bytes(), &resp); err != nil {
		t.Fatalf("unmarshal resp: %v", err)
	}
	if len(resp.Cached) != 1 || resp.Cached[0] != "http" {
		t.Fatalf("expected cached http, got %#v", resp.Cached)
	}
	if len(resp.Queued) != 0 {
		t.Fatalf("expected no queued kinds, got %#v", resp.Queued)
	}
	if !quickInserted {
		t.Fatalf("expected quick_checks insert")
	}
	if resultsInserted != 1 {
		t.Fatalf("expected cached result inserted, got %d", resultsInserted)
	}

	receivedResult := false
	for i := 0; i < 5; i++ {
		select {
		case payload := <-msgCh:
			var envelope map[string]any
			if err := json.Unmarshal([]byte(payload.Payload), &envelope); err != nil {
				continue
			}
			if envelope["type"] == "result" {
				data, _ := envelope["data"].(map[string]any)
				if data != nil && data["source"] == "cache" {
					receivedResult = true
					i = 5
					break
				}
			}
		case <-time.After(2 * time.Second):
			i = 5
		}
	}
	if !receivedResult {
		t.Fatalf("expected cached result event")
	}
}

func TestHandleExtendExtendsLeaseDeadline(t *testing.T) {
	mr, err := miniredis.Run()
	if err != nil {
		t.Fatalf("start miniredis: %v", err)
	}
	defer mr.Close()

	rdb := redis.NewClient(&redis.Options{Addr: mr.Addr(), Protocol: 2})
	defer rdb.Close()

	agentID := uuid.New()
	agentExternal := uint64(77)
	leaseUUID := uuid.New()
	leaseExternal := uint64(99)
	streamID := "stream-1"

	ctx := context.Background()
	initialDeadline := time.UnixMilli(time.Now().Add(30 * time.Second).UnixMilli())
	extendBy := 2 * time.Second
	if err := rdb.Set(ctx, leaseKey(streamID), agentID.String(), time.Until(initialDeadline)).Err(); err != nil {
		t.Fatalf("seed redis: %v", err)
	}
	initialTTL := mr.TTL(leaseKey(streamID))

	fp := &fakePool{}
	fp.queryRowFn = func(_ context.Context, query string, args ...any) pgx.Row {
		if !strings.Contains(query, "from leases") {
			t.Fatalf("unexpected query: %s", query)
		}
		if len(args) != 2 || args[0] != leaseExternal {
			t.Fatalf("unexpected query args: %#v", args)
		}
		return fakeRow{values: []any{leaseUUID, streamID, initialDeadline}}
	}
	var savedDeadline time.Time
	var execCount int
	fp.execFn = func(_ context.Context, query string, args ...any) (pgconn.CommandTag, error) {
		if !strings.Contains(query, "update leases set leased_until") {
			return pgconn.NewCommandTag(""), fmt.Errorf("unexpected exec: %s", query)
		}
		execCount++
		if len(args) != 2 {
			return pgconn.NewCommandTag(""), fmt.Errorf("unexpected exec args: %#v", args)
		}
		deadline, ok := args[0].(time.Time)
		if !ok {
			return pgconn.NewCommandTag(""), fmt.Errorf("deadline arg type %T", args[0])
		}
		savedDeadline = deadline
		if id, ok := args[1].(uuid.UUID); !ok || id != leaseUUID {
			return pgconn.NewCommandTag(""), fmt.Errorf("lease id arg %#v", args[1])
		}
		return pgconn.NewCommandTag("UPDATE 1"), nil
	}

	app := &App{pool: fp, redis: rdb}
	body, _ := json.Marshal(extendReq{AgentID: agentExternal, LeaseIDs: []uint64{leaseExternal}, ExtendByMs: uint64(extendBy.Milliseconds())})
	req := httptest.NewRequest(http.MethodPost, "/v1/agents/extend", bytes.NewReader(body))
	req = req.WithContext(context.WithValue(req.Context(), agentAuthKey{}, &agentAuth{ID: agentID, ExternalID: agentExternal}))
	w := httptest.NewRecorder()

	app.handleExtend(w, req)

	if w.Code != http.StatusOK {
		t.Fatalf("unexpected status %d: %s", w.Code, w.Body.String())
	}
	var resp extendResp
	if err := json.Unmarshal(w.Body.Bytes(), &resp); err != nil {
		t.Fatalf("unmarshal extend resp: %v", err)
	}
	if len(resp.Outcomes) != 1 {
		t.Fatalf("expected single outcome, got %d", len(resp.Outcomes))
	}
	outcome := resp.Outcomes[0]
	if outcome.LeaseID != leaseExternal {
		t.Fatalf("expected lease id %d, got %d", leaseExternal, outcome.LeaseID)
	}
	expectedDeadline := initialDeadline.Add(extendBy)
	if outcome.NewDeadlineMs != expectedDeadline.UnixMilli() {
		t.Fatalf("unexpected new deadline: got %d want %d", outcome.NewDeadlineMs, expectedDeadline.UnixMilli())
	}
	if execCount != 1 {
		t.Fatalf("expected one exec call, got %d", execCount)
	}
	if savedDeadline.UnixMilli() != expectedDeadline.UnixMilli() {
		t.Fatalf("expected db deadline %d, got %d", expectedDeadline.UnixMilli(), savedDeadline.UnixMilli())
	}
	ttl := mr.TTL(leaseKey(streamID))
	if ttl <= initialTTL {
		t.Fatalf("expected ttl to increase, initial=%v new=%v", initialTTL, ttl)
	}
}

type fakePool struct {
	queryFn    func(ctx context.Context, sql string, args ...any) (pgx.Rows, error)
	queryRowFn func(ctx context.Context, sql string, args ...any) pgx.Row
	execFn     func(ctx context.Context, sql string, args ...any) (pgconn.CommandTag, error)
}

func (f *fakePool) Close() {}

func (f *fakePool) Ping(context.Context) error { return nil }

func (f *fakePool) Query(ctx context.Context, sql string, args ...any) (pgx.Rows, error) {
	if f.queryFn != nil {
		return f.queryFn(ctx, sql, args...)
	}
	return nil, fmt.Errorf("query not implemented")
}

func (f *fakePool) QueryRow(ctx context.Context, sql string, args ...any) pgx.Row {
	if f.queryRowFn != nil {
		return f.queryRowFn(ctx, sql, args...)
	}
	return fakeRow{err: fmt.Errorf("unexpected QueryRow: %s", sql)}
}

func (f *fakePool) Exec(ctx context.Context, sql string, args ...any) (pgconn.CommandTag, error) {
	if f.execFn != nil {
		return f.execFn(ctx, sql, args...)
	}
	return pgconn.NewCommandTag(""), fmt.Errorf("unexpected Exec: %s", sql)
}

type fakeRow struct {
	values []any
	err    error
}

func (r fakeRow) Scan(dest ...any) error {
	if r.err != nil {
		return r.err
	}
	if len(dest) != len(r.values) {
		return fmt.Errorf("expected %d dest, got %d", len(r.values), len(dest))
	}
	for i, d := range dest {
		switch ptr := d.(type) {
		case *uuid.UUID:
			v, ok := r.values[i].(uuid.UUID)
			if !ok {
				return fmt.Errorf("value %d has type %T", i, r.values[i])
			}
			*ptr = v
		case *int:
			switch v := r.values[i].(type) {
			case int:
				*ptr = v
			case int64:
				*ptr = int(v)
			case int32:
				*ptr = int(v)
			default:
				return fmt.Errorf("value %d has type %T", i, r.values[i])
			}
		case *int64:
			switch v := r.values[i].(type) {
			case int64:
				*ptr = v
			case int:
				*ptr = int64(v)
			case int32:
				*ptr = int64(v)
			default:
				return fmt.Errorf("value %d has type %T", i, r.values[i])
			}
		case *string:
			v, ok := r.values[i].(string)
			if !ok {
				return fmt.Errorf("value %d has type %T", i, r.values[i])
			}
			*ptr = v
		case *time.Time:
			v, ok := r.values[i].(time.Time)
			if !ok {
				return fmt.Errorf("value %d has type %T", i, r.values[i])
			}
			*ptr = v
		case *sql.NullString:
			v, ok := r.values[i].(sql.NullString)
			if !ok {
				return fmt.Errorf("value %d has type %T", i, r.values[i])
			}
			*ptr = v
		default:
			return fmt.Errorf("unsupported dest type %T", d)
		}
	}
	return nil
}

type stubRow struct {
	kind    string
	payload []byte
	metrics []byte
}

type fakeRows struct {
	rows []stubRow
	idx  int
}

func (r *fakeRows) Close() {}

func (r *fakeRows) Err() error { return nil }

func (r *fakeRows) CommandTag() pgconn.CommandTag { return pgconn.NewCommandTag("SELECT 0") }

func (r *fakeRows) FieldDescriptions() []pgconn.FieldDescription { return nil }

func (r *fakeRows) Next() bool {
	if r.idx < len(r.rows) {
		r.idx++
		return true
	}
	return false
}

func (r *fakeRows) Scan(dest ...any) error {
	if r.idx == 0 || r.idx > len(r.rows) {
		return fmt.Errorf("scan called with no current row")
	}
	row := r.rows[r.idx-1]
	if len(dest) != 3 {
		return fmt.Errorf("expected 3 dest args, got %d", len(dest))
	}
	for _, d := range dest {
		switch ptr := d.(type) {
		case *string:
			*ptr = row.kind
		case *json.RawMessage:
			*ptr = append((*ptr)[:0], row.payload...)
		case *[]byte:
			*ptr = append((*ptr)[:0], row.metrics...)
		default:
			return fmt.Errorf("unsupported dest type %T", d)
		}
	}
	return nil
}

func (r *fakeRows) Values() ([]any, error) {
	if r.idx == 0 || r.idx > len(r.rows) {
		return nil, fmt.Errorf("values called with no current row")
	}
	row := r.rows[r.idx-1]
	return []any{row.kind, row.payload, row.metrics}, nil
}

func (r *fakeRows) RawValues() [][]byte {
	if r.idx == 0 || r.idx > len(r.rows) {
		return nil
	}
	row := r.rows[r.idx-1]
	return [][]byte{[]byte(row.kind), row.payload, row.metrics}
}

func (r *fakeRows) Conn() *pgx.Conn { return nil }

type fakeCityDB struct {
	record *geoip2.City
	err    error
}

func (f *fakeCityDB) City(net.IP) (*geoip2.City, error) { return f.record, f.err }

func (f *fakeCityDB) Close() error { return nil }

type fakeASNDB struct {
	record *geoip2.ASN
	err    error
}

func (f *fakeASNDB) ASN(net.IP) (*geoip2.ASN, error) { return f.record, f.err }

func (f *fakeASNDB) Close() error { return nil }

func TestGeoLookupResponseContainsGeoFields(t *testing.T) {
	app := &App{geo: &geoIP{
		city: &fakeCityDB{record: &geoip2.City{
			City: struct {
				Names     map[string]string `maxminddb:"names"`
				GeoNameID uint              `maxminddb:"geoname_id"`
			}{Names: map[string]string{"en": "New York"}},
			Country: struct {
				Names             map[string]string `maxminddb:"names"`
				IsoCode           string            `maxminddb:"iso_code"`
				GeoNameID         uint              `maxminddb:"geoname_id"`
				IsInEuropeanUnion bool              `maxminddb:"is_in_european_union"`
			}{IsoCode: "US"},
			Location: struct {
				TimeZone       string  `maxminddb:"time_zone"`
				Latitude       float64 `maxminddb:"latitude"`
				Longitude      float64 `maxminddb:"longitude"`
				MetroCode      uint    `maxminddb:"metro_code"`
				AccuracyRadius uint16  `maxminddb:"accuracy_radius"`
			}{Latitude: 40.7128, Longitude: -74.0060},
		}},
		asn: &fakeASNDB{record: &geoip2.ASN{AutonomousSystemNumber: 64500}},
	}}

	req := httptest.NewRequest(http.MethodGet, "/v1/geo/lookup?ip=203.0.113.5", nil)
	w := httptest.NewRecorder()
	app.handleGeoLookup(w, req)

	if w.Code != http.StatusOK {
		t.Fatalf("unexpected status %d: %s", w.Code, w.Body.String())
	}

	var payload map[string]any
	if err := json.Unmarshal(w.Body.Bytes(), &payload); err != nil {
		t.Fatalf("unmarshal response: %v", err)
	}

	geo, ok := payload["geo"].(map[string]any)
	if !ok {
		t.Fatalf("expected geo map in response, got %#v", payload["geo"])
	}
	if geo["lat"].(float64) == 0 || geo["lon"].(float64) == 0 {
		t.Fatalf("expected non-zero lat/lon, got %#v", geo)
	}
	if geo["asn"].(float64) != 64500 {
		t.Fatalf("expected ASN 64500, got %#v", geo["asn"])
	}
	if geo["country"].(string) != "US" {
		t.Fatalf("expected country US, got %#v", geo["country"])
	}
}

func TestHandleCheckGeoIncludesGeoInformation(t *testing.T) {
	checkID := uuid.New()
	fp := &fakePool{}
	fp.queryRowFn = func(_ context.Context, query string, args ...any) pgx.Row {
		if !strings.Contains(query, "from checks") {
			t.Fatalf("unexpected query: %s", query)
		}
		return fakeRow{values: []any{sql.NullString{String: "198.51.100.50", Valid: true}}}
	}
	fp.queryFn = func(_ context.Context, query string, args ...any) (pgx.Rows, error) {
		if !strings.Contains(query, "from check_results") {
			t.Fatalf("unexpected query: %s", query)
		}
		rows := []stubRow{
			{
				kind:    "http",
				payload: []byte(`{"host":"example.com","target_ip":"203.0.113.5"}`),
				metrics: []byte(`{"agent_id":"agent-1","agent_ip":"198.51.100.100"}`),
			},
			{
				kind:    "trace",
				payload: []byte(`{"hops":[{"ip":"203.0.113.1"}]}`),
				metrics: []byte(`{}`),
			},
		}
		return &fakeRows{rows: rows}, nil
	}

	app := &App{pool: fp, geo: &geoIP{
		city: &fakeCityDB{record: &geoip2.City{
			City: struct {
				Names     map[string]string `maxminddb:"names"`
				GeoNameID uint              `maxminddb:"geoname_id"`
			}{Names: map[string]string{"en": "Sample"}},
			Country: struct {
				Names             map[string]string `maxminddb:"names"`
				IsoCode           string            `maxminddb:"iso_code"`
				GeoNameID         uint              `maxminddb:"geoname_id"`
				IsInEuropeanUnion bool              `maxminddb:"is_in_european_union"`
			}{IsoCode: "US"},
			Location: struct {
				TimeZone       string  `maxminddb:"time_zone"`
				Latitude       float64 `maxminddb:"latitude"`
				Longitude      float64 `maxminddb:"longitude"`
				MetroCode      uint    `maxminddb:"metro_code"`
				AccuracyRadius uint16  `maxminddb:"accuracy_radius"`
			}{Latitude: 10.0, Longitude: 20.0},
		}},
		asn: &fakeASNDB{record: &geoip2.ASN{AutonomousSystemNumber: 64500}},
	}}

	req := httptest.NewRequest(http.MethodGet, "/v1/checks/"+checkID.String()+"/geo", nil)
	rctx := chi.NewRouteContext()
	rctx.URLParams.Add("id", checkID.String())
	req = req.WithContext(context.WithValue(req.Context(), chi.RouteCtxKey, rctx))

	w := httptest.NewRecorder()
	app.handleCheckGeo(w, req)

	if w.Code != http.StatusOK {
		t.Fatalf("unexpected status %d: %s", w.Code, w.Body.String())
	}

	var payload map[string]any
	if err := json.Unmarshal(w.Body.Bytes(), &payload); err != nil {
		t.Fatalf("unmarshal response: %v", err)
	}

	source := payload["source"].(map[string]any)
	sourceGeo := source["geo"].(map[string]any)
	if sourceGeo["lat"].(float64) == 0 || sourceGeo["asn"].(float64) != 64500 {
		t.Fatalf("unexpected source geo %#v", sourceGeo)
	}

	targets := payload["targets"].([]any)
	if len(targets) == 0 {
		t.Fatalf("expected targets in response")
	}
	targetGeo := targets[0].(map[string]any)["geo"].(map[string]any)
	if targetGeo["lat"].(float64) == 0 {
		t.Fatalf("expected target geo lat, got %#v", targetGeo)
	}

	trace := payload["trace"].(map[string]any)
	hops := trace["hops"].([]any)
	if len(hops) == 0 {
		t.Fatalf("expected trace hops")
	}
	hopGeo := hops[0].(map[string]any)["geo"].(map[string]any)
	if hopGeo["asn"].(float64) != 64500 {
		t.Fatalf("expected hop geo ASN, got %#v", hopGeo)
	}
}

func TestMapEventsSSE(t *testing.T) {
	mr, err := miniredis.Run()
	if err != nil {
		t.Fatalf("start miniredis: %v", err)
	}
	defer mr.Close()

	rdb := redis.NewClient(&redis.Options{Addr: mr.Addr(), Protocol: 2})
	defer rdb.Close()

	logger := slog.New(slog.NewTextHandler(io.Discard, &slog.HandlerOptions{Level: slog.LevelError}))
	app := &App{
		cfg:   Config{MapPubChannel: "map:events", MapStream: "map_events", MapStreamMaxLen: 128},
		redis: rdb,
		log:   logger,
	}

	router := chi.NewRouter()
	router.Get("/v1/map/events", app.handleMapEvents)

	srv := httptest.NewServer(router)
	defer srv.Close()

	req, err := http.NewRequest(http.MethodGet, srv.URL+"/v1/map/events?access_token=test", nil)
	if err != nil {
		t.Fatalf("new request: %v", err)
	}
	client := &http.Client{}
	resp, err := client.Do(req)
	if err != nil {
		t.Fatalf("sse request: %v", err)
	}
	defer resp.Body.Close()

	if ct := resp.Header.Get("Content-Type"); !strings.Contains(ct, "text/event-stream") {
		t.Fatalf("unexpected content-type: %q", ct)
	}

	reader := bufio.NewReader(resp.Body)
	lineCh := make(chan string, 4)
	errCh := make(chan error, 1)
	go func() {
		for {
			line, err := reader.ReadString('\n')
			if err != nil {
				errCh <- err
				return
			}
			lineCh <- strings.TrimSpace(line)
		}
	}()

	start := time.Now()
	go func() {
		time.Sleep(50 * time.Millisecond)
		app.publishMapEvent(context.Background(), mapEvent{Type: "check.done", Data: map[string]any{"check_id": uuid.New()}})
	}()

	timeout := time.After(2 * time.Second)
	var payload string
loop:
	for {
		select {
		case <-timeout:
			t.Fatalf("no SSE payload received")
		case err := <-errCh:
			t.Fatalf("read sse: %v", err)
		case line := <-lineCh:
			if strings.HasPrefix(line, "data: ") {
				payload = strings.TrimSpace(strings.TrimPrefix(line, "data: "))
				break loop
			}
		}
	}

	var decoded map[string]any
	if err := json.Unmarshal([]byte(payload), &decoded); err != nil {
		t.Fatalf("decode sse payload: %v", err)
	}
	if decoded["type"] != "check.done" {
		t.Fatalf("unexpected event type: %#v", decoded["type"])
	}
	if _, ok := decoded["ts"].(string); !ok {
		t.Fatalf("expected timestamp in event: %#v", decoded)
	}
	if dur := time.Since(start); dur > time.Second {
		t.Fatalf("event arrived too late: %v", dur)
	}
}

func TestMapSnapshotLimit(t *testing.T) {
	mr, err := miniredis.Run()
	if err != nil {
		t.Fatalf("start miniredis: %v", err)
	}
	defer mr.Close()

	rdb := redis.NewClient(&redis.Options{Addr: mr.Addr(), Protocol: 2})
	defer rdb.Close()

	logger := slog.New(slog.NewTextHandler(io.Discard, &slog.HandlerOptions{Level: slog.LevelError}))
	app := &App{
		cfg:   Config{MapStream: "map_events", MapStreamMaxLen: 128},
		redis: rdb,
		log:   logger,
	}

	ctx := context.Background()
	for i := 0; i < 3; i++ {
		if _, err := rdb.XAdd(ctx, &redis.XAddArgs{Stream: app.cfg.MapStream, Values: map[string]any{"json": fmt.Sprintf(`{"seq":%d}`, i)}}).Result(); err != nil {
			t.Fatalf("seed stream: %v", err)
		}
	}

	req := httptest.NewRequest(http.MethodGet, "/v1/map/snapshot?limit=2&minutes=5", nil)
	w := httptest.NewRecorder()
	app.handleMapSnapshot(w, req)

	if w.Code != http.StatusOK {
		t.Fatalf("unexpected status %d: %s", w.Code, w.Body.String())
	}

	var resp struct {
		Items      []json.RawMessage `json:"items"`
		NextCursor string            `json:"next_cursor"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &resp); err != nil {
		t.Fatalf("unmarshal snapshot: %v", err)
	}
	if len(resp.Items) != 2 {
		t.Fatalf("expected 2 items, got %d", len(resp.Items))
	}
	if strings.TrimSpace(resp.NextCursor) == "" {
		t.Fatalf("expected next cursor")
	}
}
