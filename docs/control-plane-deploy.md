# Руководство по деплою Control Plane

## Предварительные требования
- Доступная база данных, предварительно мигрированная скриптами из
  `infra/migrations`.
- Переменные окружения:
  - `CONTROL_PLANE_BIND` (адрес прослушивания, по умолчанию `0.0.0.0:8080`).
  - `CONTROL_PLANE_DATABASE_URL` (DSN SQLite/Postgres/MySQL, совместимый с SQLx).
  - `CONTROL_PLANE_POOL_SIZE` (опционально, размер пула подключений; по умолчанию
    `5`).

## Rolling restart
1. Примените миграции (идемпотентно):
   ```bash
   sqlx migrate run --source infra/migrations --database-url "$CONTROL_PLANE_DATABASE_URL"
   ```
   или `cargo run --bin control_plane -- --migrate-only`.
2. Перезапустите юнит (пример для systemd):
   ```bash
   systemctl restart dns-control-plane.service
   ```
3. Проверьте здоровье:
   - `GET /config` должен вернуть 200 и заголовок `ETag`.
   - `POST /heartbeat` от известного агента возвращает 200 и обновляет
     `agents.last_heartbeat`.

## Дашборды и алерты
- **Задержки:** отслеживайте P50/P95 по каждому маршруту (таргеты см. в
  `docs/control-plane-api.md`).
- **Ограничения скорости:** визуализируйте ответы `ERR_RATE_LIMITED` по
  эндпоинтам и агентам. Алерт, если конкретный агент выдаёт >50 429 за 5 минут.
- **Конкуренция:** стройте графики по `concurrency_windows` (поля `limit` и
  `inflight`). Триггерите алерт, если `limit` держится ниже 2 более 5 минут.
- **Свежесть heartbeat:** следите за `now() - agents.last_heartbeat`. Алерт, если
  показатель превышает 90 секунд у >5% активных агентов.

## Устранение неполадок
- **Ответы 401/403:** обновите учётные данные агента через `/register` и проверьте
  новый `auth_token` в таблице `agents`.
- **Отсутствующие лизы:** убедитесь, что новые работы добавлены в `tasks` со
  `status = 'queued'`, и проверьте дедлайны `lease_until`.
- **Накопление метрик:** убедитесь, что задача ретенции работает; фоновый
  `TimerService` логирует предупреждения при сбоях. Для ручной очистки:
  ```sql
  DELETE FROM agent_metrics WHERE recorded_at < datetime('now', '-30 days');
  DELETE FROM concurrency_windows WHERE expires_at < CURRENT_TIMESTAMP;
  ```
