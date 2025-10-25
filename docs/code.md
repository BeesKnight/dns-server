# Архитектура и код dns_agent

## Содержание
- [Запуск и main.rs](#запуск-и-mainrs)
- [Модуль control_plane](#модуль-control_plane)
- [Модуль concurrency](#модуль-concurrency)
- [Модуль dispatcher](#модуль-dispatcher)
- [Модуль lease_extender](#модуль-lease_extender)
- [Модуль resolver](#модуль-resolver)
- [Модуль runtime](#модуль-runtime)
- [Модуль workers](#модуль-workers)
- [Тестовое покрытие](#тестовое-покрытие)
- [Поток данных Claim → Report](#поток-данных-claim--report)

## Запуск и main.rs
`main.rs` инициализирует окружение агента: настраивает логирование `tracing`, читает конфигурацию прокси из переменных окружения и создаёт UDP-сокет для прослушивания запросов.【F:src/main.rs†L89-L158】 Затем инициализируется семафор `Semaphore` для ограничения числа параллельных запросов (`AGENT_MAX_INFLIGHT`) и при наличии `AGENT_CONTROL_PLANE` — HTTP-клиент control plane с bootstrap-данными (`AGENT_ID`/`AGENT_AUTH_TOKEN`).【F:src/main.rs†L45-L132】

Основной цикл читает UDP-запросы, захватывает permit из семафора и спавнит задачу `handle_request`, которая проксирует запрос к апстриму и возвращает ответ клиенту, сохраняя исходный идентификатор сообщения и измеряя задержку для логов.【F:src/main.rs†L134-L207】 Таймеры control plane создаются через `TimerService::new()` и используются для бэк-оффов claim-loop, heartbeats и продления лизов.【F:src/main.rs†L97-L198】

### Контрольная последовательность
```
Клиент -> Агент : UDP-запрос (ID N)
Агент -> Семафор : acquire()
Агент -> Upstream DNS : send_to(ID N)
Upstream DNS --> Агент : recv()
Агент -> Клиент : send_to(ID N)
Агент -> Семафор : release()
```
Эта последовательность соответствует логике `handle_request`, где на каждом запросе создаётся временный сокет, используется таймаут upstream и при таймауте операция завершается без ответа клиенту, что фиксируется логами.【F:src/main.rs†L165-L204】

Фоновая задача `run_control_plane` занимается регистрацией, heartbeats и циклом claim/extend: агент регистрируется или восстанавливает сессию, запускает тикеры heartbeats (5 секунд) и продления лизов (2 секунды), поддерживает backoff через `TimerService::sleep` при ошибках и пустых батчах.【F:src/main.rs†L200-L286】 Полученные пачки лизов передаются в диспетчер через канал `mpsc`.

## Модуль control_plane
`control_plane::mod.rs` описывает контракты обмена с control plane. Опорные типы:
- `TaskKind` и `TaskSpec` — перечисления задач и их параметров, сериализуемые в `snake_case`. Поддерживаются DNS, HTTP, TCP, Ping и Trace, причём параметры ping/trace включают опциональные поля для таймаутов и ограничения хопов.【F:src/control_plane/mod.rs†L13-L101】
- `Lease`, `ClaimResponse`, `LeaseReport`, `Observation` — структура лиза, batched-ответы и формат отчётов с произвольными наблюдениями (любое JSON-значение + опциональная единица измерения).【F:src/control_plane/mod.rs†L102-L168】
- `ClaimRequest`, `ExtendRequest`, `ReportRequest`, `HeartbeatRequest`, `RegisterRequest` — сериализуемые payload’ы, которые используются клиентом при взаимодействии с REST-эндпоинтами control plane.【F:src/control_plane/mod.rs†L148-L181】

HTTP-транспорт `HttpControlPlaneTransport` строится поверх `reqwest`, добавляет заголовки `X-Agent-Id`/`X-Agent-Token` и обрабатывает статусы, возвращая ошибки `TransportError::Http` или `TransportError::Network` в зависимости от причины.【F:src/control_plane/mod.rs†L210-L305】 Все операции реализованы через `async_trait`, поэтому верхний уровень может работать с абстрактным транспортом.

Снимок состояния клиента (см. `ControlPlaneClient::snapshot`, `heartbeat`, `claim`, `extend`, `report`) используется в `run_control_plane`: после регистрации запускаются два фоновых тикера, каждый из которых обрабатывает ошибки с логированием, а claim-цикл управляет задержками при пустых ответах, используя `TimerService::sleep` для адаптивного backoff’а.【F:src/main.rs†L200-L286】

## Модуль concurrency
`ConcurrencyController` реализует AIMD (Additive Increase, Multiplicative Decrease) контроль конкуренции на трёх уровнях: глобальном, per-kind и per-target (целевой хост/задача).【F:src/concurrency/mod.rs†L34-L198】 Статические лимиты задаются через `ConcurrencyLimits`, получаемые из конфигурации пула воркеров. Каждое успешное или неуспешное выполнение (`record_success`, `record_timeout`, `record_error`) влияет на окно: успех увеличивает ограничение на `additive_step=1`, таймаут и ошибка сокращают окно по `decrease_factor=0.5`, одновременно обновляются метрики `concurrency.backpressure` и `concurrency.inflight`. Освобождение permit происходит автоматически через `Drop` у `ConcurrencyPermit`, что предотвращает утечки разрешений.【F:src/concurrency/mod.rs†L90-L138】

## Модуль dispatcher
Диспетчер отвечает за буферизацию лизов и распределение их по специализированным очередям. Основные элементы:
- `DispatchIngress` — асинхронный интерфейс для приёма `LeaseAssignment` (лиз + `CancellationToken`). Он использует канал `mpsc` с ёмкостью `dispatch_capacity` и отдаёт подписку на backpressure через `watch::Sender<bool>`, чтобы control loop мог притормаживать claim, когда очереди переполнены.【F:src/dispatcher.rs†L42-L125】
- `DispatchQueues` — набор per-kind ресиверов (`dns`, `http`, `tcp`, `ping`, `trace`), который воркеры читают в своих подзадачах.【F:src/dispatcher.rs†L103-L110】
- `QueueTracker` — счётчик глубины очереди, обновляющий метрики `dispatcher.queue.fill` для каждого вида задач; backpressure отражается через атомарные операции и удерживает значение до следующего извлечения.【F:src/dispatcher.rs†L143-L199】

Перераспределение лизов по каналам реализуется в оставшейся части файла: диспетчер вычитывает `DispatchJob`, измеряет время ожидания, публикует в соответствующий канал и уменьшает глубину очереди. Метрики ожидания и отклонения используются в нагрузочных тестах для контроля SLA пайплайна.【F:tests/agent_pipeline.rs†L52-L126】

## Модуль lease_extender
`LeaseExtender` запускает фонового воркера, который отслеживает активные лизы и продляет их по расписанию. Команды `Track`, `Finish`, `Revoke` приходят через `mpsc`, обновляют `HashMap` с дедлайнами и отменяют `CancellationToken`, если лиз нужно прервать.【F:src/lease_extender.rs†L42-L198】 Тикер с периодом `extend_every` агрегирует идентификаторы лизов и вызывает `extend_leases`, продлевая их на `extend_by`. Ошибки продления логируются, а при остановке все токены отменяются, чтобы воркеры завершили работу корректно.【F:src/lease_extender.rs†L153-L199】

## Модуль resolver
`DnsResolver` выполняет параллельные запросы типов A/AAAA/MX/NS/TXT, кеширует результаты в `moka` с TTL и использует механизм singleflight для дедупликации одновременных запросов к одному имени.【F:src/resolver.rs†L39-L140】 При успешном ответе создаётся задача в `TimerService`, которая инвалидирует запись по истечении TTL; если апстрим возвращает усечённый ответ, резолвер автоматически переключается на TCP.【F:src/resolver.rs†L140-L222】【F:tests/resolver.rs†L162-L200】 Метрики предупреждают о превышении порога задержки выполнения таймеров или проблемах с инвалидацией кеша.

## Модуль runtime
`runtime::timer` реализует очередь таймеров на основе `BinaryHeap`, поддерживает предупреждения о глубине (`QUEUE_WARN_DEPTH`) и лаге исполнения (`LAG_WARN_THRESHOLD`). Все задачи запускаются в фоне, а `TimerService::schedule` и `TimerService::sleep` предоставляют высокоуровневый API для других подсистем.【F:src/runtime/timer.rs†L15-L116】【F:src/runtime/timer.rs†L134-L199】

`runtime::io::SocketConfigurator` абстрагирует настройку TCP/RAW-сокетов: включает `SO_REUSEADDR`, `TCP_NODELAY`, keepalive и собирает счётчики ошибок конфигурации через `metrics::counter`. Это позволяет воркерам (особенно Trace) получать корректно сконфигурированные сокеты на разных платформах.【F:src/runtime/io.rs†L5-L63】

## Модуль workers
`WorkerPoolsConfig` читает переменные `AGENT_POOL_<KIND>_SIZE` и `AGENT_TRACE_RUNTIME_WORKERS`, формируя per-kind concurrency и глобальный лимит для `ConcurrencyController`. Значения по умолчанию обеспечивают баланс между DNS и вспомогательными задачами, а trace runtime получает минимум два worker thread’а.【F:src/workers/mod.rs†L55-L124】

Каждый воркер реализует `WorkerHandler`, принимает `LeaseAssignment`, взаимодействует с `LeaseExtenderClient` (для продления), `ConcurrencyController` (для соблюдения окон) и `ReportSink` (для отправки наблюдений). Конкретные реализации (`TcpWorker`, `PingWorker`, `TraceWorker` и др.) живут в подмодулях `tcp`, `ping`, `trace`, и в тестах проверяется их способность выдерживать конкуренцию и корректно репортить результаты.【F:src/workers/mod.rs†L133-L200】【F:tests/workers.rs†L1-L200】

## Тестовое покрытие
- `tests/agent_pipeline.rs` — сценарии полного пайплайна: соблюдение протокола лизов, управление очередями диспетчера, нагрузочный тест с тысячами задач и проверкой SLA по латентности и стабильности окон конкуренции.【F:tests/agent_pipeline.rs†L1-L200】
- `tests/dispatcher.rs` — проверка backpressure, корректной доставки лизов и обработки отмен через `CancellationToken`.
- `tests/resolver.rs` — агрегация записей, поддержка EDNS, fallback на TCP и TTL-инвалидация кеша.【F:tests/resolver.rs†L50-L200】
- `tests/control_plane.rs` — сериализация/десериализация контрактов, обработка ошибок транспорта.
- `tests/workers.rs` — координация воркеров с `LeaseExtender` и `ConcurrencyController`, измерение throughput и корректности отчётов.

## Поток данных Claim → Report
1. **Claim** — цикл `run_control_plane` отправляет `ClaimRequest` с текущими ёмкостями, получает `ClaimResponse` с пачкой `Lease` и передаёт их в диспетчер через канал.【F:src/main.rs†L226-L286】【F:src/control_plane/mod.rs†L148-L169】
2. **Dispatch** — `DispatchIngress` создаёт `LeaseAssignment`, отслеживает глубину очереди и передаёт задание соответствующему воркеру по каналу, а `LeaseExtender` начинает отслеживать дедлайн лиза.【F:src/dispatcher.rs†L42-L155】【F:src/lease_extender.rs†L103-L125】
3. **Execution** — воркеры используют `ConcurrencyController` для захвата пермитов, выполняют операцию (DNS/TCP/Ping/Trace), собирают наблюдения и репортят успешное или отменённое завершение через `ReportSink`, одновременно уведомляя `LeaseExtender` о завершении.【F:src/concurrency/mod.rs†L55-L138】【F:src/workers/mod.rs†L133-L200】
4. **Report** — control plane клиент агрегирует `LeaseReport` и отправляет `ReportRequest`, после чего получает `ReportResponse.acknowledged`. В случае ошибки отчёт попадает в retry по логике клиента.【F:src/control_plane/mod.rs†L148-L169】【F:src/main.rs†L200-L286】

Эта цепочка прослеживается в интеграционных тестах `agent_pipeline`, где MockBackend проверяет, что все лизы завершаются с корректными отчётами и расширениями при длительных задачах.【F:tests/agent_pipeline.rs†L15-L120】
