create table if not exists quick_checks (
        id uuid primary key default gen_random_uuid(),
        check_id uuid not null unique references checks(id) on delete cascade,
        url text not null,
        template text,
        kinds text[] not null,
        dns_server inet,
        cache_key text not null,
        created_at timestamptz not null default now()
);

create index if not exists quick_checks_cache_key_idx on quick_checks(cache_key);
