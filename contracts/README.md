# Контракты

Этот каталог хранит совместимые, машинно-проверяемые контракты между компонентами.
Каждое изменение формата должно сопровождаться fixture и описанием версии.

Планируемая структура:

```text
door/
  frames/             # входные RS-232 пакеты и ожидаемый parse result
  snapshots/          # ZeroMQ topic/payload fixtures
session/
  manifest-v1.schema.json
  fixtures/
api/
  timeline-v1.schema.json
  timeline-v1.md
  fixtures/
```

Fixtures сначала извлекаются из проверенных Python-тестов и реальных обезличенных
данных. Новая реализация не меняет production-протокол, пока compatibility-тесты
не проходят полностью.
