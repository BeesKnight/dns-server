# Runbook dns_agent

## Содержание
- [Предварительные требования](#предварительные-требования)
- [.env и подготовка данных](#env-и-подготовка-данных)
- [Запуск docker-compose стенда](#запуск-docker-compose-стенда)
- [Запуск dns_agent](#запуск-dns_agent)
- [Взаимодействие с control plane](#взаимодействие-с-control-plane)
- [Эксплуатация Ansible плейбуков](#эксплуатация-ansible-плейбуков)
- [Проверка работоспособности](#проверка-работоспособности)
- [Дополнительные тесты и масштабирование](#дополнительные-тесты-и-масштабирование)

## Предварительные требования
Для локальной разработки и эксплуатации потребуются:
- **Rust** 1.80 или новее (указано как минимальная версия в `Cargo.toml`).【F:Cargo.toml†L1-L8】
- **Docker** и **Docker Compose** (используются для сервисов auth/sites/jobs/api-gw и зависимостей).【F:docker-compose.yml†L1-L46】
- **Python 3.11**, **Ansible 9.x**, **ansible-lint 24.x** для работы с плейбуками (см. README-ops).【F:README-ops.md†L8-L24】
- Опционально: зависимости из `requirements-ansible.txt`, если требуется фиксированная версия инструментов.【F:README-ops.md†L8-L24】

## .env и подготовка данных
1. Скопируйте шаблон общих переменных (`.env.shared`) и сервисных `.env` файлов (auth/sites/jobs/api-gw). При необходимости создайте их из примеров или согласуйте значения с командой инфраструктуры.
2. Заполните каталог `data/` файлами GeoLite2 (`GeoLite2-City.mmdb`, `GeoLite2-ASN.mmdb`). Docker-compose монтирует этот каталог в jobs-сервис; без данных часть функциональности проверок будет ограничена.【F:docker-compose.yml†L34-L52】【F:infra/roles/geoip/templates/geoip-update.sh.j2†L64-L65】
3. Для production jobs-сервиса используйте конфигурацию через переменные окружения (`HTTP_ADDR`, `DATABASE_URL`, `REDIS_ADDR`, `REDIS_PASSWORD`, `STREAM_TASKS`, `CLAIM_BLOCK_MS`, `LEASE_TTL`, `MAX_RETRIES`, `RETRY_BACKOFF`, `DEFAULT_MAX_PARALLEL`, `CACHE_TTL`, `PUB_PREFIX`, `MAP_*`, `GEOIP_*`). Эти значения считываются на старте `services/jobs-svc` и применяются ко всем зависимостям (HTTP, Postgres, Redis, GeoIP).【F:services/jobs-svc/main.go†L33-L80】【F:group_vars/jobs_svc.yml†L13-L35】
4. Проверьте, что переменные из таблицы [docs/overview.md](./overview.md#переменные-окружения) заданы в `.env` или через окружение shell перед запуском агента.

## Запуск docker-compose стенда
1. Выполните `docker compose up --build` из корня репозитория. Это соберёт production-образ `services/jobs-svc` и остальные `dev/*` сервисы, после чего поднимет зависимости.
2. Дождитесь healthchecks:
   - Postgres на порту `5432` (команда `pg_isready`).
   - Redis на порту `6379` (команда `redis-cli ping`).
   - API Gateway на порту `8088` с эндпоинтами `/livez` и `/readyz` (healthcheck curl).【F:docker-compose.yml†L3-L45】
3. Сервисы приложений доступны на портах: auth `8080`, sites `8081`, jobs `8082`, api-gw `8088`. Убедитесь, что `.env.*` файлы содержат правильные строки подключения к Postgres/Redis; jobs-сервис проверяет готовность через `/livez` и `/readyz`, которые валидируют подключение к базе и Redis перед отдачей 200 OK.【F:services/jobs-svc/main.go†L154-L189】

## Production jobs-сервис

- Код сервиса размещён в `services/jobs-svc` и использует встроенные миграции (`services/jobs-svc/migrations/*.sql`). При запуске `initApp` автоматически применяет новые версии, записывая прогресс в таблицу `schema_migrations`. Это гарантирует развёртывание таблиц `agents`, `leases`, `checks`, `check_results` и индексов для быстрого поиска просроченных лизов.【F:services/jobs-svc/main.go†L190-L237】【F:services/jobs-svc/migrate.go†L11-L71】
- HTTP-роуты `/v1/agents/register|heartbeat|claim|extend|report` соответствуют контрактам агента; ответы содержат дедлайны (`lease_duration_ms`, `next_deadline_ms`, `lease_until_ms`, `new_deadline_ms`) и возвращают коды ошибок через `http.Error` при нарушении контракта. Health-check'и `/livez` и `/readyz` всегда доступны и используются в Dockerfile/infra для проверки готовности.【F:services/jobs-svc/main.go†L86-L189】【F:services/jobs-svc/Dockerfile†L11-L18】
- В production-плейбуках Ansible переменные окружения для сервиса описаны в `group_vars/jobs_svc.yml` и `infra/inventory/group_vars/jobs_svc.yml`. Роль `services/go_app` собирает бинарь командой `go build .`, разворачивает его в `/opt/aezacheck/jobs-svc` и запускает единый процесс без дополнительных CLI-команд миграции.【F:group_vars/jobs_svc.yml†L5-L35】【F:infra/inventory/group_vars/jobs_svc.yml†L5-L35】【F:infra/roles/services/go_app/tasks/main.yml†L1-L67】

## Запуск dns_agent
1. Экспортируйте переменные окружения (можно через `.env.agent`). Минимальный набор: `AGENT_DNS_LISTEN`, `AGENT_DNS_UPSTREAM`, `AGENT_DNS_UPSTREAM_TIMEOUT_MS`, `AGENT_MAX_INFLIGHT`, а также `AGENT_CONTROL_PLANE`, `AGENT_ID`, `AGENT_AUTH_TOKEN`, если агент должен взаимодействовать с control plane.【F:src/main.rs†L27-L132】
2. (Опционально) Настройте размеры пулов воркеров через `AGENT_POOL_<KIND>_SIZE` и `AGENT_TRACE_RUNTIME_WORKERS`. См. раздел [Workers](./code.md#модуль-workers).【F:src/workers/mod.rs†L62-L118】
3. Запустите агент:
   ```bash
   cargo run --release
   ```
   или используйте собранный бинарь (`cargo build --release` → `target/release/dns_agent`). Агент выведет лог `starting DNS proxy` и начнёт слушать адрес `AGENT_DNS_LISTEN`, проксируя запросы к `AGENT_DNS_UPSTREAM` и ограничивая число одновременных запросов семафором.【F:src/main.rs†L89-L207】
4. При подключённом control plane агент автоматически выполнит bootstrap (если заданы `AGENT_ID/AGENT_AUTH_TOKEN`), зарегистрируется и запустит фоновые тикеры heartbeats и продления лизов.【F:src/main.rs†L109-L286】 Проверяйте логи `restored control plane session`, `heartbeat failed`, `lease extension failed` для диагностики.

## Взаимодействие с control plane
Контракты описаны в `README.md`. Примеры:
- Claim response (лиз) с `kind` и `spec` (DNS/HTTP/TCP/PING/TRACE).【F:README.md†L1-L28】
- Report request с массивами `completed` и `cancelled`, содержащими `observations` (значение + единица измерения).【F:README.md†L30-L41】

Для ручной проверки можно отправить HTTP-запросы:
```bash
curl -H "Content-Type: application/json" -d '{"agent_id":7,"capacities":{"dns":4}}' \
  "$AGENT_CONTROL_PLANE/claim"
```
Ответ содержит `leases`, которые агент передаст в диспетчер. После выполнения задач агент отправит `ReportRequest`, и оператор увидит агрегированные наблюдения (например, `latency_ms`, `cancelled`). Эти значения логируются и поступают в систему мониторинга через `metrics`.

## Эксплуатация Ansible плейбуков
1. Убедитесь, что `group_vars/all/vault.yml` создан и зашифрован (`ansible-vault encrypt ...`).【F:README-ops.md†L8-L33】
2. Для деплоя в окружение по умолчанию (prod):
   ```bash
   make deploy
   ```
   Команда вызывает `ansible-playbook -i inventories/prod playbooks/site.yml` и применяет роли в порядке из плейбука.【F:Makefile†L1-L12】【F:playbooks/site.yml†L1-L44】
3. Для локального стенда используйте:
   ```bash
   make deploy-local
   ```
   или dry-run без изменений:
   ```bash
   make ansible-check
   make ansible-lint
   ```
   Эти цели соответствуют шагам Source Craft pipeline и позволяют убедиться в корректности плейбуков перед пушем.【F:Makefile†L7-L12】【F:.sourcecraft/pipeline.yml†L1-L43】
4. Для доступа к secrets ansible запросит пароль vault; храните его в защищённом менеджере секретов.

## Проверка работоспособности
Быстрые проверки после запуска стенда и агента:
- `curl http://127.0.0.1:8088/livez` и `/readyz` — убедиться, что API Gateway отдаёт 200 OK.【F:docker-compose.yml†L40-L45】
- `pg_isready -h 127.0.0.1 -p 5432 -d aezacheck` — подтверждение доступности Postgres (аналогично healthcheck).【F:docker-compose.yml†L3-L19】
- `redis-cli -h 127.0.0.1 -p 6379 ping` — проверка Redis.【F:docker-compose.yml†L17-L33】
- Запустить тестовый DNS-запрос: `dig @127.0.0.1 -p 2053 example.com` и убедиться, что ответ приходит от апстрима.
- Проверить логи агента на предмет сообщений `dispatcher received batch`, `served DNS response`, `lease extension failed`.
- Убедиться, что control plane получает отчёты (`ReportResponse.acknowledged` в логах) и что активные лизы не «висят» (см. мониторинг или лог `all leases should be reported` в тестах).【F:tests/agent_pipeline.rs†L52-L126】

## Дополнительные тесты и масштабирование
- Для проверки изменений запускайте интеграционные тесты, особенно `agent_pipeline` и `resolver`, чтобы подтвердить стабильность пайплайна и TTL-кеша.【F:tests/agent_pipeline.rs†L1-L200】【F:tests/resolver.rs†L50-L200】
- Метрики backpressure (`concurrency.backpressure`, `dispatcher.queue.fill`) помогают отследить узкие места — подключите их к Loki/Prometheus через настройки `telemetry_defaults` из group_vars.【F:group_vars/all.yml†L59-L68】
- Масштабирование воркер пулов выполняйте через `AGENT_POOL_<KIND>_SIZE` и `AGENT_TRACE_RUNTIME_WORKERS`, соблюдая баланс глобального лимита `ConcurrencyController`. См. раздел [Workers](./code.md#модуль-workers).【F:src/workers/mod.rs†L62-L124】
