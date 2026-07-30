# pcrt-storage

## Назначение

`pcrt-storage` хранит видеосессию как один durable каталог и управляет её
файловым lifecycle. Он не зависит от `SQLite`, result queue, HTTP, AI, ffmpeg
или камеры. Recorder пишет артефакты через `CaptureSession`, а processor
claim-ит готовую сессию и координирует storage с result queue.

```text
capturing/<session_id>.tmp -> ready/<session_id> -> claimed/<session_id>
                                            |                 |
                                            +---------------> failed/<session_id>
```

## Формат на диске

```text
<sessions-root>/
  capturing/<session_id>.tmp/
  ready/<session_id>/
  claimed/<session_id>/
  failed/<session_id>/
```

Каждый session-directory содержит единственный `manifest.json` и видео, например
`camera-1.mkv`. Manifest contract находится в
[`contracts/session/manifest-v1.schema.json`](../../contracts/session/manifest-v1.schema.json).

Manifest содержит:

- `schema_version` и читаемый `session_id` в формате `cam-{camera_id}-{unix_ms}`;
- source ID и время capture;
- состояние и упорядоченную историю переходов;
- каждый `video` с `camera_id`, относительным filename, `codec`, `format`,
  `frame_count`, `width`, `height`, размером и SHA-256;
- причину terminal failure.

Video нельзя задать абсолютным или вложенным путём, `..`, symlink, hidden/
temporary filename или `manifest.json`. Storage не доверяет пути из JSON.

## Capture и публикация

1. Recorder вызывает `begin_capture`, получая новый `capturing/<id>.tmp`.
2. Recorder завершает ffmpeg и закрывает все файлы.
3. Recorder вызывает `finalize_capture`.
4. Recorder передаёт `codec`, `format`, frame count и frame size, полученные после
   ffmpeg. Storage проверяет regular non-empty videos, fsync-ит каждый файл и
   вычисляет SHA-256.
5. Storage atomically заменяет manifest, fsync-ит каталог и atomically
   переименовывает весь session-directory в `ready/<id>`.
6. Storage fsync-ит оба родительских state-каталога.

`ready` никогда не содержит только MKV или только metadata: сессия публикуется
одной directory-операцией на одной файловой системе. Collision target не
перезаписывается.

## Claim и processor

`claim_next_ready` проверяет checksum готовых сессий, сортирует их oldest-first
по `started_at_ms`, затем `session_id`, и atomically перемещает одну сессию в
`claimed`. Временный exclusive `.claim` marker исключает одновременный claim
двумя processor.

До необратимой обработки processor может вызвать `release_claim`. При ошибке AI
он вызывает `mark_claim_failed`, которая сохраняет причину и переносит каталог
в `failed` без удаления видео.

После успешного AI processor обязан соблюдать порядок:

```text
result_queue.insert(prepared) -> storage.delete_claimed -> result_queue.publish
```

Storage не знает SQLite и не может сам выполнять этот протокол.

## Recovery

`recover` предназначен для старта до запуска recorder/processor:

- recovery marker от atomic write или claim удаляется;
- `capturing` без final manifest переносится в `failed`, видео сохраняются;
- finalised capture, оставшийся до directory rename, публикуется в `ready`;
- ready с неверным manifest, лишним файлом, symlink, размером или SHA-256
  переносится в `failed`;
- корректный stale `claimed` возвращается в `ready`;
- повреждённый `claimed` переносится в `failed`.

Storage не выполняет destructive startup cleanup. Retention и disk budget будут
отдельной политикой только для terminal videos.

## Что перенесено из Python

Сохранено:

- явные каталоги capture/ready/processing/failed;
- oldest-first выбор сессии;
- относительное имя видео в metadata;
- publish только после успешного завершения ffmpeg.

Исправлено, а не перенесено:

- Python перемещает MKV и meta отдельными `rename`; Rust перемещает один каталог;
- Python startup script удаляет `active` и `processing`; Rust сохраняет и
  классифицирует interrupted work;
- Python metadata может направить move/delete за пределы session-root; Rust
  запрещает path traversal и symlink;
- Python не fsync-ит и не сверяет integrity; Rust фиксирует размер/SHA-256;
- Python не имеет exclusive claim/recovery; Rust использует atomic directory
  move, marker и recovery.

## Тесты

Тесты проверяют atomic publish, SHA-256, oldest-first, конкурентный claim,
recovery abandoned claim, сохранение повреждённых videos в `failed`,
interrupted capture и path traversal из manifest.
