package main

import (
	"encoding/json"
	"testing"
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
