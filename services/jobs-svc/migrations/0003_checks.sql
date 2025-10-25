create table if not exists checks (
        id uuid primary key default gen_random_uuid(),
        site_id uuid references sites(id) on delete cascade,
        status text not null,
        created_at timestamptz not null default now(),
        started_at timestamptz,
        finished_at timestamptz,
        created_by uuid references users(id),
        request_ip inet
);
