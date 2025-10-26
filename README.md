# Контрольная плоскость DNS-агентов

Репозиторий предоставляет полный HTTP control plane для DNS-агентов. Сервер
отдаёт эндпоинты `/register`, `/heartbeat`, `/claim`, `/extend`, `/report`,
`/metrics` и `/config`, описанные в документе
[`docs/control-plane-api.md`](docs/control-plane-api.md), и хранит состояние
агентов, лизы и метрики в SQL-базе данных.

## Быстрый старт

1. **Зависимости.** Для локальной разработки достаточно установленного Rust и
   SQLite (используется по умолчанию). Дополнительные сервисы не требуются.
2. **Миграции.** Примените SQL-скрипты из [`infra/migrations`](infra/migrations)
   через `sqlx` или встроенный рантайм:
   ```bash
   cargo run --bin control_plane -- --migrate-only # см. docs/control-plane-deploy.md
   ```
   либо выполните SQL-скрипты вручную.
3. **Запуск сервера.**
   ```bash
   CONTROL_PLANE_BIND=0.0.0.0:8080 \
   CONTROL_PLANE_DATABASE_URL=sqlite://control-plane.db \
   cargo run --bin control_plane
   ```
4. **Работа с API.** Используйте любой HTTP-клиент. Интеграционные тесты
   [`tests/control_plane_api.rs`](tests/control_plane_api.rs) демонстрируют как
   позитивные сценарии, так и кейсы с ограничением скорости.

По умолчанию сервер пишет структурированные трейсы через `tower-http` и хранит
данные в SQLite-файле, указанном в `CONTROL_PLANE_DATABASE_URL`.

## Схема базы данных

Схема описана в [`infra/migrations`](infra/migrations):

- `agents` — зарегистрированные агенты, их токены и дедлайны heartbeat-ов.
- `tasks`/`leases` — очередь работ и активные лизы с TTL-дедлайнами.
- `agent_metrics` — принятые метрики (хранятся 30 дней).
- `agent_configs` — подписанные конфигурации, отдаваемые через `/config`.
- `concurrency_windows` — снимки состояния диспетчера конкуренции, используемые
  `ConcurrencyController::with_persistence`.

`AgentStore` оборачивает `SqlitePool` (можно использовать любую базу,
поддерживаемую SQLx) и предоставляет помощники `register`, `heartbeat`, `claim`,
`extend`, `report`, `ingest_metrics` и `latest_config`. Фоновые задачи очищают
устаревшие метрики и просроченные окна конкуренции через общий `TimerService`.

## Аутентификация и ограничение скорости

Все эндпоинты, кроме `/register`, требуют заголовков `X-Agent-Id` и
`X-Agent-Token`. Прослойка `runtime::middleware` проверяет учётные данные через
`AgentStore::authenticate_credentials`, прикрепляет идентификатор агента к
запросу и применяет квоты для каждого эндпоинта:

| Эндпоинт    | Лимит                                      |
|-------------|--------------------------------------------|
| `/register` | 1 запрос на hostname или IP раз в 10 минут |
| `/heartbeat` | 6 в минуту (burst 3)                      |
| `/claim`    | 12 в минуту (burst 6)                      |
| `/extend`   | 30 в минуту (burst 10)                     |
| `/report`   | 12 в минуту (burst 6)                      |
| `/metrics`  | 60 в минуту (burst 10)                     |
| `/config`   | 6 в час (burst 2)                          |

Ответы 429 содержат `ERR_RATE_LIMITED` и заголовок `Retry-After`.
Ошибки аутентификации возвращают JSON-обёртки `ERR_UNAUTHORIZED`/`ERR_FORBIDDEN`
в соответствии с контрактом API.

## Наблюдаемость и мониторинг

- **Трейсинг.** HTTP-запросы публикуют спаны с методами и URI через
  `runtime::telemetry::http_trace_layer`.
- **Метрики.** `ConcurrencyController::with_persistence` записывает снимки в
  `concurrency_windows`, что позволяет строить дашборды по глубине очереди и
  доступным окнам. Метрики задержек таймеров публикуются через counters из
  `metrics`.
- **Рекомендуемые алерты:**
  - Свежесть heartbeat (`agents.last_heartbeat` > 2× таймаута).
  - Чрезмерные события `ERR_RATE_LIMITED` (по агентам).
  - Коллапс окон конкуренции (`concurrency_windows.limit < 1`).

Эндпоинт `/metrics` принимает агрегированные счётчики и gauge-метрики, которые
можно перенаправить в Prometheus или другую систему, используя сохранённые JSON.

## Тестирование и CI

Интеграционные и смоук-тесты лежат в каталоге `tests/`:

- `control_plane_api.rs` проверяет API (позитивные/негативные сценарии и простой
  тест пропускной способности).
- Дополнительные модульные тесты покрывают регистрацию агентов и персистентность
  конкуренции.

Полный набор запускается командой:

```bash
make test
```

Цель `ci` повторяет тот же шаг для интеграции с пайплайнами.

## Деплоймент

Руководства по эксплуатации и плейбуки расположены в `docs/` и `infra/`.
Отдельный гид по развёртыванию control plane см. в
[`docs/control-plane-deploy.md`](docs/control-plane-deploy.md) — в нём описаны
перезапуск службы, проверки здоровья и рекомендуемые дашборды для новых
эндпоинтов.
