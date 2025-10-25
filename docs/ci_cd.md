# Инфраструктура и CI/CD dns_agent

## Содержание
- [Структура Ansible](#структура-ansible)
- [Переменные group_vars](#переменные-group_vars)
- [Makefile и локальная автоматизация](#makefile-и-локальная-автоматизация)
- [Source Craft pipeline](#source-craft-pipeline)
- [Связь с README-ops.md](#связь-с-readme-opsmd)

## Структура Ansible
Главный плейбук `playbooks/site.yml` разбивает развёртывание на шесть стадий: инфраструктура (PostgreSQL, Redis) и сервисы (auth, sites, jobs c ролью `geoip`, api-gw, dns-agent). Каждая стадия нацелена на свою группу хостов (`database`, `cache`, `auth_svc`, и т.д.), выполняется с `become: true` и пропускает фактический деплой при `orchestration_skip_deploy=true` — это важно для dry-run в CI и локальных проверок.【F:playbooks/site.yml†L1-L44】

Роли лежат в `infra/roles/` и `infra/roles/services/*`; ключевые из них:
- `infra/postgres` и `infra/redis` — подготовка базовых сервисов хранения.
- `geoip` — загрузка GeoLite баз и настройка обновлений cron/systemd для jobs-сервиса.【F:infra/roles/geoip/templates/geoip-update.sh.j2†L64-L65】
- `services/*` — деплой сервисов платформы (включая `dns_agent`).

Инвентори разделены на `inventories/prod` и `inventories/local`, что позволяет запускать те же роли против локальных контейнеров, управляя поведением флагом `orchestration_skip_deploy` (по умолчанию `false`, но в локальных проверках включается через параметр).【F:group_vars/all.yml†L1-L13】

## Переменные group_vars
Общие переменные описаны в `group_vars/all.yml`. Они покрывают сеть, секреты, телеметрию и тюнинг производительности. Основные ключи:

| Ключ | Описание | Где используется |
| --- | --- | --- |
| `orchestration_skip_deploy` | Флаг для пропуска фактического деплоя (dry-run в CI/локально) | Плейбук `site.yml`, make-цели `ansible-check`, Source Craft pipeline【F:group_vars/all.yml†L1-L6】【F:Makefile†L7-L10】【F:.sourcecraft/pipeline.yml†L1-L43】 |
| `network.*` | Адреса сервисов (Postgres, Redis, API gateway, DNS health port, подсеть сервисов) | Экспортируются в переменные окружения для ролей сервисов【F:group_vars/all.yml†L14-L37】 |
| `secrets.*` и `vault_*` | Шифруемые значения, которые подтягиваются из `vault.yml` | Используются ролями баз и сервисов; шаблон vault описан в README-ops.md【F:group_vars/all.yml†L18-L32】【F:README-ops.md†L8-L33】 |
| `performance.*` | Лимиты ресурсов для Go-сервисов, Redis и Postgres, а также лимит `dns_agent_max_inflight` | Подхватываются ролями `services/*` и используются в Helm/systemd unit шаблонах (см. роли)【F:group_vars/all.yml†L49-L57】 |
| `telemetry_defaults.*` | Эндпоинты Loki, Redis для метрик, параметры алертов | Наследуются ролями telemetry и сервисов для настройки мониторинга【F:group_vars/all.yml†L59-L68】 |
| `geoip_data_dir` и связанные переменные | Путь до GeoLite и cron расписание обновления | Роль `geoip`, jobs-сервис и docker-compose монтирование каталога `data/`【F:group_vars/all.yml†L41-L47】【F:docker-compose.yml†L34-L52】 |

Все чувствительные данные должны попадать в `group_vars/all/vault.yml`, который шифруется через `ansible-vault`. README-ops.md содержит пошаговые инструкции по созданию и шифрованию этого файла.【F:README-ops.md†L8-L33】

## Makefile и локальная автоматизация
Makefile предоставляет четыре цели для взаимодействия с Ansible:
- `make deploy` — выполняет `ansible-playbook -i $(INVENTORY) playbooks/site.yml` против production или указанного инвентори.【F:Makefile†L1-L12】
- `make deploy-local` — развёртывает локальное окружение с включённым деплоем (`orchestration_skip_deploy=false`).【F:Makefile†L7-L9】
- `make ansible-check` — dry-run с `--check` и отключённым деплоем, используется в CI.【F:Makefile†L9-L10】
- `make ansible-lint` — запуск `ansible-lint` для статической проверки ролей.【F:Makefile†L10-L12】

Эти цели повторяют шаги из Source Craft pipeline и README-ops.md, позволяя разработчикам быстро проверить плейбуки локально перед пушем.

## Source Craft pipeline
Конвейер `.sourcecraft/pipeline.yml` заменяет GitHub Actions и запускается на Source Craft при изменениях плейбуков, ролей, `group_vars`, инвентори, `ansible.cfg`, Makefile и самого пайплайна.【F:.sourcecraft/pipeline.yml†L1-L30】 Последовательность шагов повторяет проверочный сценарий:
1. Шаг `Checkout` синхронизирует репозиторий через `sourcecraft/checkout@v1`.
2. Шаг `Set up Python 3.11` поднимает окружение на образе `ubuntu-22.04`, устанавливает Python 3.11, создаёт виртуальное окружение и ставит `ansible>=9.0.0` вместе с `ansible-lint>=24.2.0`.
3. Шаг `Run ansible-lint` запускает линтер по `playbooks/site.yml`.
4. Шаг `Dry-run local inventory` выполняет `ansible-playbook -i inventories/local playbooks/site.yml --check -e orchestration_skip_deploy=true`, чтобы удостовериться, что роли применимы и без фактического деплоя.【F:.sourcecraft/pipeline.yml†L31-L43】

Пайплайн повторяет логику прежнего GitHub Actions, но выполняется на ранне-конфигурируемом агенте Source Craft и обеспечивает единый результат lint + dry-run перед слиянием.

## Связь с README-ops.md
`README-ops.md` дополняет документацию:
- **Prereqs**: Python 3.11, Ansible 9.x, Ansible-lint 24.x; опциональные зависимости можно установить через `requirements-ansible.txt`.
- **Vault**: создание `group_vars/all/vault.yml`, шифрование `ansible-vault encrypt`, требования к паролю при запуске плейбуков.
- **CI pipeline**: описание соответствия Source Craft конвейера целям `make ansible-check` и `make ansible-lint`.
- **Rollback и мониторинг**: инструкция по откату на стабильный git-тег и проверке health сервисов после развёртываний.

Ключевые пункты вынесены в этот документ, полный гайд доступен по ссылке [`README-ops.md`](../README-ops.md).【F:README-ops.md†L1-L88】 Для оперативной эксплуатации см. также runbook [docs/runbook.md](./runbook.md).
