package main

import (
	"context"
	"embed"
	"fmt"
	"sort"
	"strings"

	"github.com/jackc/pgx/v5/pgxpool"
)

//go:embed migrations/*.sql
var migrationFS embed.FS

func migrate(ctx context.Context, db *pgxpool.Pool) error {
	if _, err := db.Exec(ctx, `create table if not exists schema_migrations (name text primary key, applied_at timestamptz not null default now())`); err != nil {
		return fmt.Errorf("init migrations table: %w", err)
	}

	entries, err := migrationFS.ReadDir("migrations")
	if err != nil {
		return fmt.Errorf("list migrations: %w", err)
	}
	sort.Slice(entries, func(i, j int) bool { return entries[i].Name() < entries[j].Name() })

	for _, entry := range entries {
		name := entry.Name()
		var applied bool
		if err := db.QueryRow(ctx, `select exists(select 1 from schema_migrations where name=$1)`, name).Scan(&applied); err != nil {
			return fmt.Errorf("check migration %s: %w", name, err)
		}
		if applied {
			continue
		}

		raw, err := migrationFS.ReadFile("migrations/" + name)
		if err != nil {
			return fmt.Errorf("read migration %s: %w", name, err)
		}
		script := strings.TrimSpace(string(raw))
		if script == "" {
			if _, err := db.Exec(ctx, `insert into schema_migrations(name) values($1)`, name); err != nil {
				return fmt.Errorf("mark empty migration %s: %w", name, err)
			}
			continue
		}

		tx, err := db.Begin(ctx)
		if err != nil {
			return fmt.Errorf("begin migration %s: %w", name, err)
		}

		for _, stmt := range splitStatements(script) {
			if stmt == "" {
				continue
			}
			if _, err := tx.Exec(ctx, stmt); err != nil {
				_ = tx.Rollback(ctx)
				return fmt.Errorf("exec migration %s: %w", name, err)
			}
		}
		if err := tx.Commit(ctx); err != nil {
			return fmt.Errorf("commit migration %s: %w", name, err)
		}
		if _, err := db.Exec(ctx, `insert into schema_migrations(name) values($1)`, name); err != nil {
			return fmt.Errorf("mark migration %s: %w", name, err)
		}
	}
	return nil
}

func splitStatements(script string) []string {
	pieces := strings.Split(script, ";")
	out := make([]string, 0, len(pieces))
	for _, stmt := range pieces {
		trimmed := strings.TrimSpace(stmt)
		if trimmed == "" {
			continue
		}
		out = append(out, trimmed)
	}
	return out
}
