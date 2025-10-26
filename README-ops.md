# Руководство по эксплуатации DNS-платформы

Этот документ описывает подготовку и работу с DNS-платформой с помощью
Ansible-плейбуков из репозитория.

## Предварительные требования

* **Python**: версия 3.11 или новее.
* **Ansible**: ветка 9.x (устанавливается через `pip install ansible`).
* **Ansible-lint**: версия 24.x (`pip install ansible-lint`).
* Дополнительные инструменты можно установить командой
  `pip install -r requirements-ansible.txt`, если нужно закрепить версии
  зависимостей.

Перед запуском плейбуков убедитесь, что файл `group_vars/all/vault.yml`
существует и зашифрован `ansible-vault`. Шаблон можно получить копированием
`group_vars/all/vault.yml.example`, после чего выполнить:

```bash
cp group_vars/all/vault.yml.example group_vars/all/vault.yml
ansible-vault encrypt group_vars/all/vault.yml
```

Во время исполнения плейбуков Ansible запросит пароль от vault.

## Структура репозитория

```
playbooks/site.yml           # главный сценарий оркестрации
inventories/                 # окружения и переопределения групп
  prod/                      # продакшен-хосты и overrides
  local/                     # локальные параметры разработки
infra/roles/                 # роли Ansible, используемые в плейбуках
group_vars/                  # общие переменные для всех окружений
ansible.cfg                  # настройки Ansible по умолчанию
Makefile                     # вспомогательные цели автоматизации
```

## Деплой в продакшен

Используйте цель `deploy` (или вызовите `ansible-playbook` напрямую), чтобы
применить конфигурацию продакшена:

```bash
make deploy
# эта команда запускает
ansible-playbook -i inventories/prod playbooks/site.yml
```

Плейбук сначала разворачивает инфраструктурные роли (`infra/postgres`,
`infra/redis`), затем выкладывает каждый Go-сервис (`services/auth-svc`,
`services/sites-svc`, `services/jobs-svc`, `services/api-gw`,
`services/dns-agent`) и предварительно заливает GeoIP-артефакты.

### Локальная оркестрация

Для локального тестирования предусмотрен отдельный inventory. По умолчанию он
пропускает реальные изменения, чтобы CI и локальные прогоны были безопасны:

```bash
ansible-playbook -i inventories/local playbooks/site.yml \
  -e orchestration_skip_deploy=true
```

Чтобы применить роли в песочнице, установите
`orchestration_skip_deploy=false` и убедитесь, что необходимые сервисы
(PostgreSQL, Redis и т.д.) доступны локально.

### CI-пайплайн

Workflow `.github/workflows/ansible.yml` автоматически запускает линтер и
выполняет dry-run против локального inventory при каждом пуше и pull request, где
меняются инфраструктурные файлы. Это повторяет цели `make ansible-check` и
`make ansible-lint`.

## Переменные окружения и секреты

Общие настройки описаны в `group_vars/all.yml`. В файле содержатся:

* Сетевые адреса баз данных, кешей и HTTP-эндпоинтов.
* Секреты, защищённые vault (`vault_postgres_password`, `vault_redis_password`,
  `vault_jwt_signing_key`, `vault_jobs_maxmind_license_key`).
* Параметры производительности, используемые ролями.
* Значения по умолчанию для телеметрии DNS-агентов.

Продакшеновый control plane (jobs-сервис) собирается из `services/jobs-svc` и
полностью настраивается через переменные окружения, которые рендерятся из
`group_vars/jobs_svc.yml` (и переопределений inventory). Роль Ansible выполняет
`go build .` и выкладывает единый бинарь с встроенными миграциями, поэтому
отдельная команда `migrate` не нужна.【F:group_vars/jobs_svc.yml†L5-L35】【F:infra/inventory/group_vars/jobs_svc.yml†L5-L35】【F:services/jobs-svc/migrate.go†L11-L71】

Все чувствительные значения должны храниться в зашифрованном файле
`group_vars/all/vault.yml`. Любое окружение может переопределить переменные,
добавив файлы в `inventories/<env>/group_vars/`.

## План отката

1. Определите последнюю стабильную метку или коммит для инфраструктурных
   плейбуков.
2. Повторите деплой из этого ревизии:
   ```bash
   git checkout <stable-ref>
   ansible-playbook -i inventories/prod playbooks/site.yml
   ```
3. Если откат требует изменения конфигурации (например, отключения неудачного
   релиза сервиса), обновите соответствующие group vars и снова выполните
   `make deploy`.
4. После восстановления окружения вернитесь на актуальную ветку и подготовьте
   фикс, закрывающий причину сбоя, прежде чем пытаться деплой снова.

После отката обязательно проверьте здоровье приложений (API gateway, воркеры
jobs, DNS-агенты) и просмотрите телеметрические дашборды.
