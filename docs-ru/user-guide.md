# Руководство пользователя NORA

**Версия:** 0.2.32
**Дата:** 2026-03-16
**Правообладатель:** ООО «ТАИАРС» (торговая марка АРТАИС)

---

## 1. Общие сведения

NORA — реестр артефактов для команд разработки. Программа обеспечивает управление и кэширование библиотек, Docker-образов и иных программных компонентов, используемых при сборке приложений.

Данное руководство предназначено для разработчиков, которые используют NORA в качестве источника зависимостей.

---

## 2. Настройка рабочего окружения

npm и PyPI используют именованные репозитории. Получите URL в `/ui/repositories`
или у администратора. `/npm/` и `/simple/` ниже — примеры явно настроенных alias,
а не пути по умолчанию; `/npm-local/` — отдельный local для публикации. Группы
выбирают один источник пакета и не объединяют версии. См. [руководство по
репозиториям](../docs/repositories.md).

### 2.1. npm / Node.js

Укажите NORA в качестве реестра:

```bash
npm config set registry http://nora.example.com:4000/npm
```

Или создайте файл `.npmrc` в корне проекта:

```
registry=http://nora.example.com:4000/npm
```

Команды используют выбранный endpoint. Local отдаёт опубликованные пакеты, proxy обращается к своему upstream, group выбирает одного участника. Сохранение новых файлов зависит от `save_files`; успешное скачивание не гарантирует завершённую запись кэша.

### 2.2. Docker

Адрес выбирает именованный репозиторий: `local` принимает публикации, `proxy` кэширует один upstream и отклоняет запись. В примере администратор настроил local на `:4000`, proxy Docker Hub на `:5001`. Адреса и команды доступны в `/ui/repositories?format=docker`.

```bash
docker login nora.example.com:4000
docker login nora.example.com:5001
docker pull nora.example.com:5001/library/nginx:latest
docker push nora.example.com:4000/myteam/myapp:1.0.0
```

Для прямого HTTP-подключения настройте Docker `insecure-registries` для выбранных адресов либо используйте reverse proxy с TLS. Номер порта сам по себе не включает TLS. См. [Docker-репозитории и порты](../docs/docker-repositories.md).

### 2.3. Maven

Добавьте репозиторий в `settings.xml`:

```xml
<mirrors>
  <mirror>
    <id>nora</id>
    <mirrorOf>central</mirrorOf>
    <url>http://nora.example.com:4000/maven2</url>
  </mirror>
</mirrors>
```

### 2.4. Python / pip

```bash
pip install --index-url http://nora.example.com:4000/simple flask
```

Или в `pip.conf`:

```ini
[global]
index-url = http://nora.example.com:4000/simple
```

### 2.5. Cargo / Rust

Настройка в `~/.cargo/config.toml`:

```toml
[registries.nora]
index = "sparse+http://nora.example.com:4000/cargo/"

[source.crates-io]
replace-with = "nora"
```

### 2.6. Helm

Helm использует OCI-протокол через Docker Registry API:

```bash
helm push mychart-0.1.0.tgz oci://nora.example.com:4000/helm
helm pull oci://nora.example.com:4000/helm/mychart --version 0.1.0
```

---

## 3. Публикация пакетов

### 3.1. npm

```bash
npm publish --registry http://nora.example.com:4000/npm-local/
```

Требования:
- Файл `package.json` с полями `name` и `version`.
- Каждая версия публикуется однократно. Повторная публикация той же версии запрещена.

### 3.2. Docker

Публикуйте только в local-репозиторий; ниже используется его адрес `:4000`. Proxy на другом порту остаётся независимым и не принимает push.

```bash
docker tag myapp:latest nora.example.com:4000/myteam/myapp:1.0.0
docker push nora.example.com:4000/myteam/myapp:1.0.0
```

### 3.3. Maven

```bash
mvn deploy -DaltDeploymentRepository=nora::default::http://nora.example.com:4000/maven2
```

### 3.4. Raw (произвольные файлы)

```bash
# Загрузка
curl -X PUT --data-binary @release.tar.gz http://nora.example.com:4000/raw/builds/release-1.0.tar.gz

# Скачивание
curl -O http://nora.example.com:4000/raw/builds/release-1.0.tar.gz
```

---

## 4. Работа в изолированной среде

Если сборочный сервер не имеет доступа к сети Интернет, используйте предварительное кэширование.

### 4.1. Кэширование зависимостей проекта

На машине с доступом к Интернету и NORA выполните:

```bash
npm ci --ignore-scripts --no-audit --registry http://nora.example.com:4000/npm/
```

Lockfile должен ссылаться на NORA: фиксированные внешние URL могут обходить выбранный registry. Для офлайн-работы требуется `save_files=true` и завершённая запись файлов в кэш. Старые npm/yarn/pip-команды `nora mirror` не поддерживают именованные цели и возвращают явную ошибку.

### 4.2. Предварительная загрузка конкретной версии

```bash
npm pack lodash@4.17.21 --registry http://nora.example.com:4000/npm/
```

Повторите команду для нужных версий. Это не автоматическая загрузка всех версий upstream.

---

## 5. Веб-интерфейс

NORA предоставляет веб-интерфейс для просмотра содержимого реестра:

```
http://nora.example.com:4000/ui/
```

Доступные функции:
- Просмотр списка артефактов по протоколам.
- Количество версий и размер каждого пакета.
- Журнал последних операций.
- Метрики загрузок.

---

## 6. Документация API

Интерактивная документация API доступна по адресу:

```
http://nora.example.com:4000/api-docs
```

Формат: OpenAPI 3.0 (Swagger UI).

---

## 7. Аутентификация

Если администратор включил аутентификацию, для операций записи требуется токен.

### 7.1. Получение токена

```bash
curl -u admin:password http://nora.example.com:4000/auth/token
```

### 7.2. Использование токена

```bash
# npm
npm config set //nora.example.com:4000/npm/:_authToken TOKEN
npm config set //nora.example.com:4000/npm-local/:_authToken TOKEN

# Docker: отдельно для каждого используемого endpoint
docker login nora.example.com:4000
docker login nora.example.com:5001

# curl
curl -H "Authorization: Bearer TOKEN" http://nora.example.com:4000/npm/my-package
```

Операции чтения по умолчанию не требуют аутентификации (роль `read` назначается автоматически).

---

## 8. Часто задаваемые вопросы

**В: Что произойдёт, если внешний реестр (npmjs.org) станет недоступен?**
О: Доступны только сохранённые файлы, прошедшие текущие проверки источника, целостности и политик. Файлы с незавершённой/неудачной записью и никогда не сохранённые файлы недоступны. Предварительно загрузите нужные версии клиентами через явный alias и проверьте офлайн-доступ.

**В: Можно ли публиковать приватные пакеты?**
О: Да. npm-пакеты публикуются напрямую в local-репозиторий; proxy и group не принимают публикацию. Доступ регулируется существующей аутентификацией, ролями и ограничениями namespace, а не отдельными ACL репозиториев.

**В: Как обновить кэш метаданных?**
О: Метаданные прокси обновляются при запросах после истечения `metadata_ttl` (по умолчанию 300 секунд). При необходимости задайте `metadata_ttl=0` в нужном proxy и перезапустите NORA. Не удаляйте произвольный `metadata.json`: опубликованные данные и метаданные разных репозиториев изолированы.

**В: Поддерживаются ли scoped-пакеты npm (@scope/package)?**
О: Да, полностью. Например: `npm install @babel/core --registry http://nora.example.com:4000/npm`.
