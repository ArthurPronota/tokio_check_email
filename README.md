# Описание кода проверки email

## Общая картина

Функция `check_emails` принимает JSON-строку с массивом объектов (например, пользователей), проверяет email-адреса, фильтрует невалидные и неактивные записи, кэширует домены в двух `HashSet` (валидные и невалидные), и возвращает **отсортированный, дедуплицированный список нормализованных email-адресов**.

## Пошаговый разбор

### 1. Сигнатура и результат

```rust
async fn check_emails(
    json_in: &str,
    valid_hosts: Arc<RwLock<HashSet<String>>>,
    invalid_hosts: Arc<RwLock<HashSet<String>>>,
) -> Result<Vec<String>, anyhow::Error>
```

- `json_in` — входная JSON-строка;
- `valid_hosts` и `invalid_hosts` — общие кэши доменов под `Arc<RwLock<...>>`, чтобы их можно было безопасно использовать из нескольких задач;
- возвращает `Vec<String>` — нормализованные email-адреса, прошедшие проверку;
- ошибки — через `anyhow::Error`.

### 2. Парсинг JSON

```rust
let array = match serde_json::from_str::<Value>(json_in)? {
    Value::Array(a) => a,
    oth => {
        let oth_str = match oth { /* ... */ };
        error!("json_in isn't Array, it's: {oth_str}");
        return Err(anyhow::anyhow!("json_in isn't Array, it's: {oth_str}"));
    },
};
```

- JSON парсится в `serde_json::Value`.
- Ожидается **массив**. Если пришло что-то другое — логируется тип и возвращается ошибка.
- Вложенный `match` вручную определяет имя типа значения (Array, Bool, Null, Number, Object, String) для информативного сообщения.

### 3. Лёгкий валидатор

```rust
let em_light_validator = emval::EmailValidator {
    deliverable_address: false,
    ..Default::default()
};
```

- Создаётся валидатор с `deliverable_address: false` — то есть **без проверки DNS/SMTP**, только синтаксис и нормализация.
- Это быстрая проверка, которая не ходит в сеть.

### 4. Потоковая обработка массива

```rust
let mut stream_iter = stream::iter(array.iter());

while let Some(value) = stream_iter.next().await {
    // ...
}
```

- `futures::stream::iter` превращает итератор в асинхронный поток.
- `while let Some(...) = stream_iter.next().await` — цикл по элементам.
- Сейчас это не даёт параллелизма, но структура готова к тому, чтобы позже заменить на `for_each_concurrent`.

### 5. Фильтр по `active`

```rust
if !value.get("active").and_then(|a| a.as_bool()).unwrap_or(false) {
    continue;
}
```

- Берётся поле `active`.
- Если оно не `true` (или отсутствует, или не bool) — запись пропускается.
- `unwrap_or(false)` — безопасный дефолт.

### 6. Извлечение email

```rust
let em_opt = value.get("email").and_then(|e| e.as_str());

match em_opt {
    None => continue,
    Some(em_str) => { /* ... */ }
}
```

- Если поля `email` нет или оно не строка — пропуск.
- Иначе — переходим к валидации.

### 7. Лёгкая валидация

```rust
match em_light_validator.validate_email(em_str) {
    Err(err) => {
        error!("{em_str}: {err}");
        continue;
    },
    Ok(e_v_l) => { /* ... */ }
}
```

- Синтаксически невалидный email — логируется и пропускается.
- `e_v_l` содержит, среди прочего, `domain_name` и `normalized` (нормализованная форма).

### 8. Проверка кэшей доменов

```rust
if invalid_hosts.read().await.contains(&e_v_l.domain_name) {
    continue;
}

if !valid_hosts.read().await.contains(&e_v_l.domain_name) {
    // ...
}
```

- Если домен уже в **чёрном списке** — сразу пропуск.
- Если домен ещё **не в белом списке** — нужна полная проверка (см. ниже).
- Если домен уже в белом списке — пропускаем дорогую проверку и сразу добавляем email в результат.

### 9. Дорогая проверка в `spawn_blocking`

```rust
let em_w = em_str.to_string();
match spawn_blocking(move || emval::validate_email(&em_w)).await {
    Ok(v_all) => {
        match v_all {
            Ok(_) => {
                valid_hosts.write().await.insert(e_v_l.domain_name);
            },
            Err(err) => {
                invalid_hosts.write().await.insert(e_v_l.domain_name);
                error!("{em_str}: {err}");
                continue;
            },
        }
    },
    Err(err) => {
        error!("{err}");
        continue;
    }
}
```

Ключевые моменты:

- **`spawn_blocking`** — потому что `emval::validate_email` может делать блокирующие операции (DNS-запросы, SMTP). Их нельзя выполнять прямо в async-контексте, иначе заблокируется весь рабочий поток Tokio.
- `em_w = em_str.to_string()` — клонирование строки, потому что замыкание `move` забирает владение, а `em_str` — это `&str` из JSON.
- При успехе домен **добавляется в `valid_hosts`** (белый список) — чтобы в следующий раз не проверять его снова.
- При ошибке домен добавляется в `invalid_hosts` (чёрный список) и email пропускается.
- Если сам `spawn_blocking` упал (например, паника) — логируется и пропуск.

### 10. Добавление в результат

```rust
vec_out.push(e_v_l.normalized);
```

- В результат идёт **нормализованная** форма email (например, с приведённым к нижнему регистру доменом).
- Обратите внимание: даже если email уже был в белом списке домена, он всё равно добавляется — дедупликация будет позже.

### 11. Постобработка

```rust
vec_out.sort();
vec_out.dedup();

Ok(vec_out)
```

- Сортировка и удаление дубликатов.
- Возврат `Ok(vec_out)`.

## Логика кэширования доменов

| Состояние домена | Действие |
|---|---|
| В `invalid_hosts` | Пропустить email |
| В `valid_hosts` | Добавить email без проверки |
| Ни там, ни там | Полная проверка через `spawn_blocking`, затем запись в один из кэшей |

Это оптимизация: если в JSON много адресов с одинаковым доменом (например, `@gmail.com`), дорогая проверка выполняется **один раз** на домен.

## Разбор примера из `main`

```json
[
  { "id": 1, "email": "user@mail.com", "active": true },
  { "id": 2, "email": null, "active": true },
  { "id": 3, "email": "invalid", "active": false },
  { "id": "wrong", "email": "test@test.com", "active": true },
  { "random": "data" }
]
```

Что произойдёт с каждой записью:

| # | Запись | `active` | `email` | Результат |
|---|---|---|---|---|
| 1 | `user@mail.com` | true | строка | Проходит лёгкую валидацию → проверка домена → в результат |
| 2 | `email: null` | true | null | `em_opt = None` → пропуск |
| 3 | `invalid` | **false** | строка | Пропуск на шаге `active` |
| 4 | `test@test.com` | true | строка | Проходит лёгкую валидацию → проверка домена → в результат |
| 5 | нет `email` | нет `active` | — | Пропуск на шаге `active` |

Итог: `["test@test.com", "user@mail.com"]` (после сортировки).

## Что здесь хорошо

- **Двухуровневая валидация**: быстрая (`emval::EmailValidator`) и дорогая (`emval::validate_email`).
- **Кэш доменов**: дорогая проверка выполняется один раз на домен.
- **`spawn_blocking`**: блокирующие DNS/SMTP-вызовы не блокируют async-runtime.
- **Обработка ошибок**: каждое место логируется и продолжает работу, а не падает.
- **Дедупликация и сортировка** в конце.

## Что можно улучшить

1. **`stream::iter` + `while let`** — это последовательная обработка. Можно заменить на `for_each_concurrent` или `buffer_unordered`, чтобы проверять несколько email параллельно.
2. **Двойное чтение `RwLock`** — `invalid_hosts.read().await` и `valid_hosts.read().await` берут блокировки дважды. Можно объединить или использовать `try_read`.
3. **`spawn_blocking` внутри цикла** — можно вынести батчами, если доменов много.
4. **`e_v_l.normalized`** — стоит убедиться, что `emval` действительно нормализует (например, lower-case домена), иначе дедупликация может не сработать.
5. **Ошибка парсинга JSON** — сейчас просто `?`, без логирования. Можно добавить контекст через `.context(...)` из `anyhow`.

## Итог

Код делает **асинхронную проверку email-адресов из JSON** с:

- фильтрацией по `active`;
- двухуровневой валидацией (лёгкая → тяжёлая);
- кэшированием доменов в `HashSet` под `RwLock`;
- выносом блокирующих проверок в `spawn_blocking`;
- сортировкой и дедупликацией результата.

Архитектурно это правильный паттерн для задач, где дорогая проверка (DNS/SMTP) сочетается с большим количеством входных данных и повторяющимися доменами.
