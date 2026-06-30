# Dev Vault

Obsidian vault для заметок по коду и проектам. Управляется в т.ч. через Claude Code.

Структуру, конвенции и правила см. в [CLAUDE.md](./CLAUDE.md).

## Slash-команды (Claude Code)
- `/daily` — daily-заметка на сегодня, перенос незакрытых задач со вчера
- `/process-inbox` — разобрать `00-inbox/`
- `/new-project <name>` — завести проект в `10-projects/`
- `/adr <project> <название>` — новый ADR с автонумерацией
- `/extract-atomic <заметка>` — вынести атомарные идеи в `20-reference/`
- `/audit-links` — orphans, битые ссылки, дубликаты
