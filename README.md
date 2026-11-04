# onec_assistant — MVP оркестратора ИИ-ассистента для 1С

Сервис принимает вопрос пользователя, отправляет его языковой модели вместе с описанием
доступных функций, сам исполняет запрошенные моделью вызовы функций чтения 1С
(с проверкой прав в коде), возвращает итоговый ответ с источниками и пишет журнал в SQLite.

Модель ничего не считает сама: числа берутся только из результатов функций.
Итоговые суммы (например, общий остаток по неоплаченным счетам) считает код.

## Принципы

| Правило | Где обеспечивается |
|---|---|
| Числа и факты только из результатов функций; нет данных — «не нашёл» | Системный промпт; код проверяет, что значимые числа ответа есть в данных, иначе `warnings: unverified_numbers` и запись в журнал |
| Неоднозначный контрагент — переспросить, а не выбирать | Промпт; `search_counterparty` возвращает `ambiguous`; код отклоняет вызов с `counterparty_id` одного из кандидатов неоднозначного поиска в том же запросе (`ambiguous_counterparty`) |
| Результаты функций — данные, а не инструкции | Промпт; результаты передаются только в сообщениях роли `tool` в обёртке `{"status":"ok","data":...}`; права проверяет код, поэтому «команда» в данных не может их расширить |
| Каждый ответ содержит источник | Блок «Источники» дописывает код по фактически выполненным вызовам (функция, параметры, период, статус); то же в поле `sources` |
| Права проверяет код | Модель видит только разрешённые роли функции, а реестр повторно проверяет роль перед каждым вызовом |
| Секреты не попадают в журнал и логи | `Secret` скрывает значения в `Debug`/`Display`; журнал вырезает секреты конфигурации из всех текстовых полей; ошибки HTTP без URL |
| Только чтение | Функций записи нет ни в реестре, ни в `OneCClient` |

## Быстрый старт

Проверено на Rust 1.95 (edition 2021); минимальная поддерживаемая версия не определялась.

```bash
cp .env.example .env
```

```bash
cp users.example.toml users.toml
```

Поднимите OpenAI-совместимый сервер с поддержкой вызова функций. Для llama-server это
обычно запуск с шаблоном чата, поддерживающим tools (флаг `--jinja`), для vLLM — с включённым
автоматическим выбором функций и парсером вызовов под вашу модель. Точные флаги сверьте
с документацией вашей версии сервера.

```bash
set -a && source .env && set +a && cargo run --release
```

Проверка:

```bash
curl -s http://127.0.0.1:3000/health
```

```bash
curl -s -X POST http://127.0.0.1:3000/chat -H 'content-type: application/json' -d '{"user_id":"ivanova","message":"Какое сальдо с Вектором на 1 ноября?"}'
```

Для YandexGPT или GigaChat задайте `LLM_BACKEND` и ключи (см. «Провайдеры моделей»).
По умолчанию `ONEC_BACKEND=mock`: ответы строятся на тестовых данных
(см. «Тестовые данные»).

## Конфигурация

Все параметры задаются переменными окружения; полный список с комментариями — в `.env.example`.

| Переменная | По умолчанию | Назначение |
|---|---|---|
| `BIND_ADDR` | `127.0.0.1:3000` | Адрес HTTP-сервиса |
| `USERS_FILE` | `users.toml` | Пользователи и роли |
| `AUDIT_DB_PATH` | `audit.sqlite` | Файл журнала |
| `LOG_QUESTIONS` | `true` | `false` — текст вопроса в журнал не пишется |
| `LLM_BACKEND` | `openai_compatible` | `openai_compatible`, `yandexgpt` или `gigachat` |
| `LLM_BASE_URL` | зависит от провайдера | См. «Провайдеры моделей» |
| `LLM_MODEL` | `local-model` / `yandexgpt/latest` / `GigaChat-2-Pro` | Имя модели (идёт в запрос и в журнал) |
| `LLM_API_KEY` | — | openai_compatible: Bearer-ключ (необязательно); yandexgpt: API-ключ сервисного аккаунта; gigachat: ключ авторизации |
| `LLM_TIMEOUT_SECS` | `60` | Таймаут одного HTTP-запроса к модели |
| `LLM_TEMPERATURE` | `0` | Температура (yandexgpt: 0–1; gigachat: 0 — не передавать) |
| `LLM_MAX_TOKENS` | `2000` | Лимит токенов ответа (yandexgpt, gigachat) |
| `LLM_FOLDER_ID` | — | yandexgpt: идентификатор каталога |
| `GIGACHAT_SCOPE` | `GIGACHAT_API_PERS` | gigachat: `GIGACHAT_API_PERS`, `GIGACHAT_API_B2B` или `GIGACHAT_API_CORP` |
| `GIGACHAT_OAUTH_URL` | `https://ngw.devices.sberbank.ru:9443/api/v2/oauth` | gigachat: получение токена |
| `LLM_CA_CERT_PATH` | — | Дополнительные корневые сертификаты (PEM), например НУЦ Минцифры |
| `ONEC_BACKEND` | `mock` | `mock` или `http` |
| `ONEC_BASE_URL` | — | Обязателен при `http`. Используйте `https://`: при `http://` с паролем сервис предупреждает при старте, что пароль уходит открытым текстом |
| `ONEC_USER`, `ONEC_PASSWORD` | — | Basic-аутентификация HTTP-сервиса 1С |
| `ONEC_TIMEOUT_SECS` | `10` | Таймаут одного обращения к 1С (и одного вызова функции) |
| `ONEC_PATH_*` | `/hs/assistant/...` | Пути эндпоинтов (условные) |
| `MAX_ITERATIONS` | `5` | Максимум обращений к модели на один вопрос. Вызовы функций, запрошенные на последнем шаге, в 1С не отправляются (результат уже не использовать) и пишутся в журнал со статусом `not_executed` |
| `MAX_TOOL_CALLS_PER_ITERATION` | `4` | Сверх лимита вызовы не исполняются, модель получает ошибку `too_many_calls` |
| `REQUEST_TIMEOUT_SECS` | `60` | Общий таймаут обработки вопроса |
| `MAX_MESSAGE_CHARS` | `4000` | Максимальная длина вопроса |
| `RUST_LOG` | `info` | Уровень логов |

## Провайдеры моделей

Все провайдеры реализуют один трейт `LlmProvider`; цикл вызова функций, права и журнал
от провайдера не зависят. Конфигурация проверяется при старте: без ключа, каталога
(YandexGPT) или с неверным scope (GigaChat) сервис не запустится.

### OpenAI-совместимый сервер (`openai_compatible`)

llama-server, vLLM и подобные: `POST {LLM_BASE_URL}/chat/completions` с `tools`.
Учтены расхождения серверов: отсутствующий `id` вызова и `arguments` объектом вместо строки.

### YandexGPT (`yandexgpt`)

Нативный REST API Yandex AI Studio: `POST {LLM_BASE_URL}/completion`
(по умолчанию `https://ai.api.cloud.yandex.net/foundationModels/v1/completion`).

- Аутентификация: `Authorization: Api-Key <LLM_API_KEY>` (API-ключ сервисного аккаунта
  с ролью `ai.languageModels.user`), заголовок `x-folder-id` при заданном `LLM_FOLDER_ID`.
- Модель: `modelUri = gpt://<LLM_FOLDER_ID>/<LLM_MODEL>` или полный `gpt://...` в `LLM_MODEL`.
- Вызовы функций приходят в `toolCallList` без id; результаты отправляются сообщением
  `role: user` с `toolResultList` и сопоставляются по имени функции.
- Ответ принимается и в обёртке `result` (как в руководстве), и без неё (как в справочнике).

Сверено с документацией ([вызов функции](https://aistudio.yandex.ru/docs/ru/ai-studio/operations/generation/function-call),
[справочник Completion](https://aistudio.yandex.ru/docs/ru/ai-studio/text-generation/api-ref/TextGeneration/completion),
[аутентификация](https://aistudio.yandex.ru/docs/ru/ai-studio/api-ref/authentication)), ноябрь 2026.

**Противоречие в документации:** справочник у поля `tools` пишет, что оно «пока не
поддерживается и будет проигнорировано», а руководство по вызову функций использует именно
его. Перед боевым запуском проверьте на реальном ключе, что модель возвращает `toolCallList`.
Если нет — модель будет отвечать без данных 1С, и это будет видно по пустым `sources`.

### GigaChat (`gigachat`)

REST API v1: `POST {LLM_BASE_URL}/chat/completions` (по умолчанию `https://api.giga.chat/v1`).

- Токен доступа получается по `GIGACHAT_OAUTH_URL` (`Authorization: Basic <LLM_API_KEY>`,
  `RqUID: <uuid4>`, тело `scope=<GIGACHAT_SCOPE>`), кэшируется до истечения
  (`expires_at`, не дольше 30 минут, с запасом в минуту) и обновляется один раз при ответе 401.
- Функции передаются в `functions` с `function_call: "auto"`; вызов приходит в
  `message.function_call` вместе с `functions_state_id`, который возвращается в истории.
  Результат — сообщение `role: function` с `content` в виде JSON-объекта в строке.
- `temperature` у GigaChat строго больше 0: при `LLM_TEMPERATURE=0` поле не передаётся.
- Если TLS-соединение с серверами Сбера не устанавливается, укажите корневой сертификат
  НУЦ Минцифры в `LLM_CA_CERT_PATH`.

Сверено со [спецификацией OpenAPI GigaChat API](https://developers.sber.ru/docs/files/openapi/gigachat/api.yml)
и руководствами ([авторизация](https://developers.sber.ru/docs/ru/gigachat/api/authorization),
[работа с функциями](https://developers.sber.ru/docs/ru/gigachat/api/function-calling)), ноябрь 2026.

Оба провайдера проверены только против фейковых серверов, повторяющих документированный
формат (`tests/providers.rs`); на реальных ключах не проверялись.

## Роли

`users.toml`:

```toml
[[users]]
id = "ivanova"
role = "accountant"
```

| Функция | viewer | manager | accountant |
|---|:-:|:-:|:-:|
| `search_counterparty` | ✓ | ✓ | ✓ |
| `list_unpaid_invoices` | | ✓ | ✓ |
| `get_counterparty_balance` | | | ✓ |
| `get_account_turnover` | | | ✓ |

Источник ролей подключается через трейт `auth::IdentityResolver`; сейчас реализация
`FileIdentityResolver`, позже — Битрикс24.

## API

### `POST /chat`

Запрос:

```json
{"user_id": "petrov", "message": "Что не оплатил Сидоров?", "conversation_id": null}
```

`conversation_id` необязателен: без него начинается новый диалог, его id возвращается в ответе.

Ответ `200`:

```json
{
  "request_id": "5f0c…",
  "conversation_id": "a1b2…",
  "answer": "У ИП Сидоров один неоплаченный счёт СЧ-0011 на 15750.00 RUB.\n\nИсточники:\n- 1С: search_counterparty(query=Сидоров)\n- 1С: list_unpaid_invoices(counterparty_id=cp-004)",
  "sources": [
    {"function": "search_counterparty", "arguments": {"query": "Сидоров"}, "period": null, "status": "ok"},
    {"function": "list_unpaid_invoices", "arguments": {"counterparty_id": "cp-004"}, "period": null, "status": "ok"}
  ],
  "warnings": []
}
```

`sources` содержит все вызовы, включая неуспешные (`status` — код ошибки: `forbidden`,
`invalid_arguments`, `unknown_tool`, `ambiguous_counterparty`, `timeout`, `not_found`,
`too_many_calls`, `result_too_large`, `backend_error`).
Вызовы со статусом `not_executed` (последний шаг при исчерпании лимита) видны только в журнале:
такой запрос завершается ошибкой `iteration_limit`.

Ошибки — `{"error": {"code": "...", "message": "..."}}`:

| HTTP | code |
|---|---|
| 400 | `invalid_request`, `invalid_user_id`, `empty_message`, `message_too_long`, `invalid_conversation_id` |
| 403 | `unknown_user` |
| 404 | `conversation_not_found` (в т.ч. диалог другого пользователя) |
| 502 | `llm_error`, `iteration_limit` |
| 504 | `timeout` |

Подробности ошибок модели и 1С наружу не отдаются — они в журнале и логах.

### `GET /health`

`{"status": "ok"}`

## Журнал

Таблица `audit_log` в SQLite. На каждый вопрос — итоговая строка (`function_name IS NULL`)
и по строке на каждый вызов функции; строки связаны `request_id`. Запросы неизвестных
пользователей и отклонённые запросы тоже журналируются.

Поля: `ts`, `request_id`, `conversation_id`, `user_id`, `role`, `question` (только в итоговой
строке; `NULL` при `LOG_QUESTIONS=false`), `function_name`, `arguments`, `status`,
`result_size` (байт, переданных модели), `model`, `duration_ms`, `error`
(для итоговой строки — текст ошибки или предупреждения).

```bash
sqlite3 audit.sqlite "select ts, user_id, function_name, status, duration_ms from audit_log order by id desc limit 20"
```

Перед записью из текстовых полей вырезаются `ONEC_PASSWORD` и `LLM_API_KEY`,
длинные значения обрезаются до 2000 символов.

## Функции 1С (только чтение)

| Функция | Параметры | Результат |
|---|---|---|
| `search_counterparty` | `query` (2–100 символов, название или ИНН) | `count`, `ambiguous`, `candidates[{id, name, inn}]` |
| `get_counterparty_balance` | `counterparty_id`, `date` (YYYY-MM-DD) | `balance` (> 0 — должны нам, < 0 — должны мы), `currency` |
| `get_account_turnover` | `account`, `date_from`, `date_to` | сальдо на начало/конец, обороты Дт/Кт |
| `list_unpaid_invoices` | `counterparty_id` | `invoices[]`, `total_outstanding` (считает код) |

Параметры проверяются по JSON Schema до вызова; кривой JSON, лишние поля, неверные даты
и `date_from > date_to` возвращаются модели как `invalid_arguments`.
Суммы передаются строками с двумя знаками (`"125000.50"`), внутри хранятся в копейках.

### Контракт HTTP-сервиса 1С (условный, требует согласования)

`GET {ONEC_BASE_URL}{path}?{параметры}`, Basic-аутентификация, ответ JSON:

| Путь (переменная) | Параметры | Ответ |
|---|---|---|
| `ONEC_PATH_SEARCH_COUNTERPARTY` | `query` | `[{"id","name","inn"}]` |
| `ONEC_PATH_COUNTERPARTY_BALANCE` | `counterparty_id`, `date` | `{"counterparty_id","counterparty_name","date","balance","currency"}` |
| `ONEC_PATH_ACCOUNT_TURNOVER` | `account`, `date_from`, `date_to` | `{"account","date_from","date_to","opening_balance","debit_turnover","credit_turnover","closing_balance","currency"}` |
| `ONEC_PATH_UNPAID_INVOICES` | `counterparty_id` | `[{"number","date","due_date","amount","paid","outstanding","currency"}]` |

Суммы принимаются строкой или числом; 404 трактуется как «не найдено»; ответы больше 2 МБ отклоняются.

### Тестовые данные (`ONEC_BACKEND=mock`)

| id | Контрагент | ИНН | Сальдо |
|---|---|---|---|
| cp-001 | ООО «Ромашка» | 7701234567 | 125000.50 |
| cp-002 | ООО «Ромашка-Плюс» | 7702345678 | -48300.00 |
| cp-003 | АО «Вектор» | 7803456789 | 0.00 |
| cp-004 | ИП Сидоров Пётр Ильич | 500100732259 | 15750.00 |
| cp-005 | ООО «Альфа» [с «системной командой» в названии] | 7704567890 | 9999.99 |

Счета: 62.01, 60.01, 51. Неоплаченные счета есть у cp-001 (2 шт.) и cp-004 (1 шт.).

## Разработка

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

Тесты:

- `tests/orchestrator.rs` — сценарии на `MockProvider` + `MockOneC`: успех, запрет по роли,
  неизвестная функция, неверные параметры, таймаут 1С, общий таймаут, лимит итераций,
  лимит вызовов за итерацию, неоднозначный контрагент, инъекция в данных, придуманные числа,
  чужой диалог;
- `tests/audit.rs` — записи журнала, `LOG_QUESTIONS=false`, журналирование ошибок,
  отсутствие секретов в файле базы;
- `tests/api.rs` — интеграционный тест `POST /chat` и `GET /health` на реальном сервере;
- `tests/openai.rs` — полный цикл через `OpenAiCompatible` против фейкового сервера;
- `tests/providers.rs` — полный цикл через `YandexGpt` и `GigaChat` против фейковых серверов:
  заголовки, формат истории, повторное использование и обновление токена GigaChat;
- `tests/onec_http.rs` — `HttpOneC`: Basic-аутентификация, 401/404, таймаут;
- модульные тесты в `src/` (валидатор схем, деньги, конфигурация, извлечение чисел и др.).

Структура:

```
src/
  config.rs            конфигурация из env, Secret
  error.rs             ошибки запуска
  llm/                 LlmProvider, OpenAiCompatible, YandexGpt, GigaChat, MockProvider
  onec/                OneCClient, MockOneC, HttpOneC, доменные типы
  tools/               реестр функций, валидатор JSON Schema, функции 1С
  auth/                роли, IdentityResolver, FileIdentityResolver
  audit/               журнал SQLite
  orchestrator/        цикл вызова функций, промпт, история диалогов, проверка чисел
  api/                 axum: /chat, /health
  main.rs              сборка сервиса
```

## Ограничения MVP и известные риски

- **`user_id` не аутентифицируется.** Он берётся из тела запроса. Сервис нельзя выставлять
  наружу без шлюза, который подтверждает пользователя; это задача интеграции с Битрикс24.
- **История диалогов в памяти процесса** (до 1000 диалогов по 40 сообщений): после
  перезапуска теряется, при нескольких экземплярах сервиса не разделяется.
- **Проверка чисел — эвристика.** Числа, вычисленные моделью (суммы, разности), и числа,
  записанные словами, не проверяются как данные; предупреждение не блокирует ответ.
- **Защита от выбора среди похожих контрагентов строгая:** сравнить двух похожих
  контрагентов в одном вопросе нельзя, пока пользователь не уточнит.
- **Сбой записи журнала не блокирует ответ** (пишется в лог с уровнем error). Если журнал
  обязателен для комплаенса, поменяйте на отказ в ответе.
- Вызовы функций в одной итерации исполняются последовательно.
- id вызовов уникальны в пределах диалога: если провайдер id не присылает (YandexGPT) или
  повторяет его, оркестратор назначает свой (`call_<uuid>`).
- Ответ модели об отказе в доступе формулирует сама модель; факт отказа фиксирует код.

## TODO

- [ ] Проверить `YandexGpt` на реальном ключе, в первую очередь работу `tools` (см. противоречие в документации).
- [ ] Проверить `GigaChat` на реальном ключе, в том числе TLS к `ngw.devices.sberbank.ru` и `api.giga.chat`.
- [ ] YandexGPT: при необходимости добавить аутентификацию IAM-токеном с автообновлением (сейчас только API-ключ).
- [ ] Согласовать контракт HTTP-сервиса 1С (`src/onec/http.rs`) и поправить пути/типы.
- [ ] `IdentityResolver` для Битрикс24 и аутентификация запросов.
- [ ] Хранилище диалогов вне памяти процесса, если нужно несколько экземпляров.
