create table if not exists check_results (
        id uuid primary key default gen_random_uuid(),
        check_id uuid references checks(id) on delete cascade,
        kind text not null,
        status text not null,
        payload jsonb not null,
        metrics jsonb,
        stream_id text,
        created_at timestamptz not null default now()
);
