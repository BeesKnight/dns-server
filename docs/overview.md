# Обзор проекта dns_agent

## Содержание
- [Контекст и назначение](#контекст-и-назначение)
- [Структура репозитория](#структура-репозитория)
- [Переменные окружения](#переменные-окружения)
- [Внешние зависимости](#внешние-зависимости)
- [Дальнейшее чтение](#дальнейшее-чтение)

## Контекст и назначение
Агент `dns_agent` используется в кейсе Aeza Check для проверки доступности инфраструктуры: он выступает DNS-прокси между внутренними сервисами и внешними ресолверами, а также обрабатывает задания, выдаваемые control plane. Контрольная плоскость распределяет лизы с параметрами проверки (DNS, HTTP, TCP, ping, trace), агент регистрируется, поддерживает heartbeats и возвращает отчёты с наблюдениями по результатам проверок.【F:src/main.rs†L24-L132】【F:src/control_plane/mod.rs†L13-L170】

## Структура репозитория
- `src/` — исходный код агента на Rust, включая модули прокси, работы с control plane, конкуренции, диспетчера, таймеров, резолвера и воркеров.【F:src/main.rs†L19-L163】【F:src/workers/mod.rs†L19-L200】
- `tests/` — интеграционные и нагрузочные тесты, проверяющие пайплайн агента, очереди диспетчера, TTL-кеш резолвера и поведения воркеров.【F:tests/agent_pipeline.rs†L1-L160】【F:tests/resolver.rs†L50-L160】
- `playbooks/` — основной Ansible-плейбук `site.yml` и сценарии развёртывания инфраструктуры Aeza (PostgreSQL, Redis, API GW, DNS-агенты).【F:playbooks/site.yml†L1-L44】
- `infra/roles/` — роли Ansible для инфраструктуры (Postgres, Redis, GeoIP) и сервисов платформы.
- `group_vars/` и `inventories/` — общие переменные и инвентори для prod/local окружений с сетевыми параметрами и секретами.【F:group_vars/all.yml†L1-L61】
- `dev/` — Dockerfile и вспомогательные сервисы для локального стенда (auth, sites, jobs, api-gw).
- `data/` — каталог для размещения GeoLite баз, которые монтируются в docker-compose окружение.【F:docker-compose.yml†L34-L52】
- `Makefile` — цели автоматизации запуска плейбуков и проверок.【F:Makefile†L1-L12】
- `README.md` и `README-ops.md` — контракты control plane и операционные инструкции соответственно.【F:README.md†L1-L41】【F:README-ops.md†L1-L76】

## Переменные окружения
| Переменная | Назначение | Значение/дефолт |
| --- | --- | --- |
| `AGENT_DNS_LISTEN` | Адрес, на котором UDP-прокси принимает запросы | `127.0.0.1:2053`【F:src/main.rs†L27-L56】 |
| `AGENT_DNS_UPSTREAM` | Адрес upstream DNS сервера | `127.0.0.1:5354`【F:src/main.rs†L27-L56】 |
| `AGENT_DNS_UPSTREAM_TIMEOUT_MS` | Таймаут ответа апстрима в миллисекундах | `2500` мс (по умолчанию)【F:src/main.rs†L39-L55】 |
| `AGENT_MAX_INFLIGHT` | Максимум параллельных DNS-запросов через семафор | `2048` (минимум 1)【F:src/main.rs†L45-L55】 |
| `AGENT_CONTROL_PLANE` | URL control plane для регистрации и получения лизов | Требуется задать вручную; при отсутствии агент работает только как DNS-прокси【F:src/main.rs†L109-L132】 |
| `AGENT_ID` | Фиксированный идентификатор агента для повторного bootstrap | Нет дефолта, используется в паре с токеном【F:src/main.rs†L60-L87】 |
| `AGENT_AUTH_TOKEN` | Токен аутентификации control plane | Нет дефолта, должен идти вместе с `AGENT_ID`【F:src/main.rs†L60-L87】 |
| `AGENT_POOL_<KIND>_SIZE` | Размер пула воркеров для конкретного вида задач (DNS/HTTP/TCP/PING/TRACE) | По умолчанию: DNS=4, HTTP=2, TCP=2, PING=2, TRACE=1【F:src/workers/mod.rs†L62-L118】 |
| `AGENT_TRACE_RUNTIME_WORKERS` | Количество потоков runtime для trace-воркеров | `2` (минимум 1)【F:src/workers/mod.rs†L62-L111】 |

Дополнительные переменные `AGENT_POOL_*` и `AGENT_TRACE_RUNTIME_WORKERS` используются вместе с параметрами телеметрии и backpressure, описанными в [обзоре кода](./code.md#workers).

## Внешние зависимости
| Библиотека | Назначение |
| --- | --- |
| `tokio` | Асинхронный runtime с поддержкой UDP/TCP, таймеров и многопоточности агента【F:Cargo.toml†L16-L21】 |
| `reqwest` | HTTP-клиент для взаимодействия с control plane (REST+JSON с TLS)【F:Cargo.toml†L13-L21】 |
| `serde` / `serde_json` | Сериализация/десериализация контрактов control plane и отчётов【F:Cargo.toml†L13-L21】 |
| `moka` | Асинхронный кеш с TTL для DNS-резолвера и отложенной инвалидации【F:Cargo.toml†L12-L21】 |
| `crossbeam-queue` / `tokio-util` | Очереди и синхронизация диспетчера и воркеров【F:Cargo.toml†L10-L21】 |
| `tracing` / `tracing-subscriber` | Структурированное логирование и фильтры уровня выполнения【F:Cargo.toml†L16-L21】 |
| `metrics` | Экспорт метрик по конкуренции, очередям, таймерам и сокетам【F:Cargo.toml†L17-L24】 |
| `socket2` / `pnet_packet` | Низкоуровневые операции с сокетами (keepalive, raw sockets) и парсинг сетевых пакетов для trace/ping【F:Cargo.toml†L16-L27】 |
| `anyhow` / `thiserror` | Обработка ошибок в асинхронных подсистемах агента【F:Cargo.toml†L9-L20】 |

## Дальнейшее чтение
- Подробная архитектура модулей и потоков данных описана в [docs/code.md](./code.md).
- Инфраструктура, CI/CD и автоматизация Ansible рассмотрены в [docs/ci_cd.md](./ci_cd.md).
- Пошаговый runbook для локального стенда и эксплуатации — в [docs/runbook.md](./runbook.md).
