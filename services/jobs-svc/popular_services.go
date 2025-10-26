package main

import (
	"context"
	"crypto/sha1"
	"database/sql"
	"encoding/hex"
	"errors"
	"fmt"
	"math"
	"strings"
	"time"

	"github.com/google/uuid"
	"github.com/jackc/pgx/v5"
	"github.com/redis/go-redis/v9"
)

type popularServiceCatalog struct {
	ID          string
	Name        string
	URL         string
	BaseRating  float64
	BaseReviews int
}

func (a *App) popularServiceLoop(ctx context.Context) {
	interval := a.cfg.PopularCheckInterval
	if interval <= 0 {
		return
	}
	ticker := time.NewTicker(interval)
	defer ticker.Stop()

	a.runPopularServiceChecks(ctx)

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			a.runPopularServiceChecks(ctx)
		}
	}
}

func (a *App) runPopularServiceChecks(ctx context.Context) {
	runCtx, cancel := context.WithTimeout(ctx, 15*time.Second)
	defer cancel()

	rows, err := a.pool.Query(runCtx,
		`select id, name, url, base_rating, base_reviews from popular_services_catalog where is_active=true order by name`)
	if err != nil {
		a.log.Warn("popular_catalog_query_failed", "err", err)
		return
	}
	defer rows.Close()

	now := time.Now().UTC()
	for rows.Next() {
		var svc popularServiceCatalog
		if err := rows.Scan(&svc.ID, &svc.Name, &svc.URL, &svc.BaseRating, &svc.BaseReviews); err != nil {
			a.log.Warn("popular_catalog_scan_failed", "err", err)
			continue
		}

		if a.shouldSkipPopularService(runCtx, svc.ID, now) {
			continue
		}

		if err := a.schedulePopularServiceCheck(runCtx, svc); err != nil {
			if !errors.Is(err, errPopularDedup) {
				a.log.Warn("popular_schedule_failed", "service_id", svc.ID, "err", err)
			}
		}
	}
	if err := rows.Err(); err != nil {
		a.log.Warn("popular_catalog_rows_failed", "err", err)
	}
}

var errPopularDedup = errors.New("popular service dedup")

func (a *App) shouldSkipPopularService(ctx context.Context, id string, now time.Time) bool {
	var last sql.NullTime
	if err := a.pool.QueryRow(ctx,
		`select max(scheduled_at) from popular_service_runs where service_id=$1`, id).Scan(&last); err != nil {
		a.log.Warn("popular_last_run_failed", "service_id", id, "err", err)
		return true
	}
	if !last.Valid {
		return false
	}
	interval := a.cfg.PopularCheckInterval
	if interval <= 0 {
		interval = 5 * time.Minute
	}
	if now.Sub(last.Time) < interval {
		return true
	}
	return false
}

func (a *App) schedulePopularServiceCheck(ctx context.Context, svc popularServiceCatalog) error {
	spec, err := buildQuickSpec("http", quickTargetInfo{URL: svc.URL})
	if err != nil {
		return err
	}
	cacheKey := fmt.Sprintf("popular:%s", svc.ID)
	sum := sha1.Sum(spec)
	dupKey := fmt.Sprintf("last:%s:%s", cacheKey, hex.EncodeToString(sum[:]))
	ttl := a.cfg.QuickDedupTTL
	if ttl <= 0 {
		ttl = 30 * time.Second
	}
	if ok, err := a.redis.SetNX(ctx, dupKey, "1", ttl).Result(); err != nil {
		return err
	} else if !ok {
		return errPopularDedup
	}

	var checkID uuid.UUID
	if err := a.pool.QueryRow(ctx,
		`insert into checks(site_id,status,created_by,request_ip) values ($1,$2,$3,$4) returning id`,
		nil, "queued", nil, nil).Scan(&checkID); err != nil {
		return err
	}

	_, err = a.pool.Exec(ctx,
		`insert into quick_checks(check_id,url,template,kinds,dns_server,cache_key) values ($1,$2,$3,$4,$5,$6)`,
		checkID, svc.URL, "quick", []string{"http"}, nil, cacheKey)
	if err != nil {
		return err
	}

	if _, err := a.pool.Exec(ctx,
		`insert into popular_service_runs(check_id, service_id) values ($1,$2)`, checkID, svc.ID); err != nil {
		return err
	}

	taskID := uuid.New()
	payload := map[string]any{"task_id": taskID.String(), "check_id": checkID.String(), "kind": "http", "spec": string(spec)}
	streamID, err := a.redis.XAdd(ctx, &redis.XAddArgs{Stream: a.cfg.StreamTasks, Values: payload}).Result()
	if err != nil {
		_, _ = a.pool.Exec(ctx, `delete from popular_service_runs where check_id=$1`, checkID)
		_, _ = a.pool.Exec(ctx, `delete from quick_checks where check_id=$1`, checkID)
		_, _ = a.pool.Exec(ctx, `delete from checks where id=$1`, checkID)
		return err
	}
	a.observeStreamWrite(ctx, a.cfg.StreamTasks, streamID, fmt.Sprintf("popular:%s", svc.ID))
	return nil
}

func (a *App) refreshPopularSnapshot(ctx context.Context, checkID uuid.UUID) {
	if err := a.updatePopularServiceSnapshot(ctx, checkID); err != nil {
		if !errors.Is(err, pgx.ErrNoRows) {
			a.log.Warn("popular_snapshot_failed", "check_id", checkID, "err", err)
		}
	}
}

func (a *App) updatePopularServiceSnapshot(ctx context.Context, checkID uuid.UUID) error {
	var serviceID string
	if err := a.pool.QueryRow(ctx,
		`select service_id from popular_service_runs where check_id=$1`, checkID).Scan(&serviceID); err != nil {
		return err
	}

	window := a.cfg.PopularSnapshotWindow
	if window <= 0 {
		window = 24 * time.Hour
	}
	windowStr := fmt.Sprintf("%f seconds", window.Seconds())

	var lastStatus string
	var lastCheck time.Time
	err := a.pool.QueryRow(ctx,
		`select cr.status, cr.created_at
                   from check_results cr
                   join popular_service_runs ps on ps.check_id = cr.check_id
                  where ps.service_id=$1
                  order by cr.created_at desc
                  limit 1`, serviceID).Scan(&lastStatus, &lastCheck)
	if err != nil && !errors.Is(err, pgx.ErrNoRows) {
		return err
	}
	hasLast := err == nil

	rows, err := a.pool.Query(ctx,
		`select date_trunc('hour', cr.created_at) as bucket,
                        count(*) as total,
                        count(*) filter (where lower(cr.status) in ('ok','done','success')) as ok_count
                   from check_results cr
                   join popular_service_runs ps on ps.check_id = cr.check_id
                  where ps.service_id=$1 and cr.created_at >= now() - $2::interval
                  group by 1`, serviceID, windowStr)
	if err != nil {
		return err
	}
	defer rows.Close()

	type bucketStat struct {
		total   int64
		success int64
	}
	stats := make(map[time.Time]bucketStat)
	var successTotal, totalCount int64
	for rows.Next() {
		var bucket time.Time
		var total int64
		var success int64
		if err := rows.Scan(&bucket, &total, &success); err != nil {
			rows.Close()
			return err
		}
		bucket = bucket.UTC()
		stats[bucket] = bucketStat{total: total, success: success}
		successTotal += success
		totalCount += total
	}
	if err := rows.Err(); err != nil {
		return err
	}

	now := time.Now().UTC().Truncate(time.Hour)
	start := now.Add(-23 * time.Hour)
	hourly := make([]int, 24)
	for i := 0; i < 24; i++ {
		bucket := start.Add(time.Duration(i) * time.Hour)
		if entry, ok := stats[bucket]; ok {
			hourly[i] = int(entry.total)
		}
	}

	rating := 0.0
	if totalCount > 0 {
		ratio := float64(successTotal) / float64(totalCount)
		rating = math.Round(math.Min(math.Max(ratio*5.0, 0), 5)*10) / 10
	}

	staleAfter := a.cfg.PopularSnapshotStale
	if staleAfter <= 0 {
		staleAfter = 30 * time.Minute
	}
	status := derivePopularStatus(lastStatus, hasLast, lastCheck, staleAfter)

	var lastCheckVal any
	if hasLast {
		lastCheckVal = lastCheck.UTC()
	}

	hourlyPG := make([]int32, len(hourly))
	for i, v := range hourly {
		hourlyPG[i] = int32(v)
	}

	_, err = a.pool.Exec(ctx,
		`insert into popular_service_snapshots(service_id,status,rating,sample_count,hourly_counts,updated_at,last_check_at)
                 values ($1,$2,$3,$4,$5,now(),$6)
                 on conflict(service_id) do update set
                        status=excluded.status,
                        rating=excluded.rating,
                        sample_count=excluded.sample_count,
                        hourly_counts=excluded.hourly_counts,
                        updated_at=now(),
                        last_check_at=excluded.last_check_at`,
		serviceID, status, rating, int(totalCount), hourlyPG, lastCheckVal)
	return err
}

func derivePopularStatus(raw string, hasLast bool, last time.Time, staleAfter time.Duration) string {
	if !hasLast {
		return "unknown"
	}
	status := "error"
	switch strings.ToLower(strings.TrimSpace(raw)) {
	case "ok", "done", "success":
		status = "ok"
	case "cancelled", "canceled", "timeout":
		status = "warn"
	default:
		status = "error"
	}
	if staleAfter > 0 {
		if last.IsZero() || time.Since(last) > staleAfter {
			return "unknown"
		}
	}
	return status
}
