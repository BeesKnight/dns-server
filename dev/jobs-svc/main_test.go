package main

import (
	"bytes"
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
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

func TestHandleExtendExtendsLeaseDeadline(t *testing.T) {
	mr, err := miniredis.Run()
	if err != nil {
		t.Fatalf("start miniredis: %v", err)
	}
	defer mr.Close()

	rdb := redis.NewClient(&redis.Options{Addr: mr.Addr()})
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
		asn: &fakeASNDB{record: &geoip2.ASN{AutonomousSystemNumber: 64500, AutonomousSystemOrganization: "Example ISP"}},
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
	if org, ok := geo["asn_org"].(string); !ok || org != "Example ISP" {
		t.Fatalf("expected ASN org Example ISP, got %#v", geo["asn_org"])
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
		asn: &fakeASNDB{record: &geoip2.ASN{AutonomousSystemNumber: 64500, AutonomousSystemOrganization: "Example ISP"}},
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
	if org, ok := sourceGeo["asn_org"].(string); !ok || org != "Example ISP" {
		t.Fatalf("unexpected source geo asn_org %#v", sourceGeo["asn_org"])
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
	if org, ok := hopGeo["asn_org"].(string); !ok || org != "Example ISP" {
		t.Fatalf("expected hop geo ASN org, got %#v", hopGeo["asn_org"])
	}
}
