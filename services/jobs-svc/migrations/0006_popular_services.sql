create table if not exists popular_services_catalog (
        id text primary key,
        name text not null,
        url text not null,
        base_rating double precision not null default 4.5,
        base_reviews integer not null default 0,
        is_active boolean not null default true,
        created_at timestamptz not null default now()
);

insert into popular_services_catalog (id, name, url, base_rating, base_reviews)
values
        ('yt', 'YouTube', 'https://youtube.com', 4.7, 2381),
        ('gg', 'Google', 'https://google.com', 4.7, 2210),
        ('tg', 'Telegram', 'https://telegram.org', 4.6, 1975),
        ('dc', 'Discord', 'https://discord.com', 4.2, 1120),
        ('gh', 'GitHub', 'https://github.com', 4.9, 980)
on conflict (id) do update
        set name = excluded.name,
            url = excluded.url,
            base_rating = excluded.base_rating,
            base_reviews = excluded.base_reviews,
            is_active = true;

create table if not exists popular_service_runs (
        check_id uuid primary key references checks(id) on delete cascade,
        service_id text not null references popular_services_catalog(id) on delete cascade,
        scheduled_at timestamptz not null default now()
);

create index if not exists idx_popular_service_runs_service on popular_service_runs(service_id, scheduled_at desc);

create table if not exists popular_service_snapshots (
        service_id text primary key references popular_services_catalog(id) on delete cascade,
        status text not null,
        rating double precision not null,
        sample_count integer not null,
        hourly_counts integer[] not null,
        updated_at timestamptz not null default now(),
        last_check_at timestamptz
);
