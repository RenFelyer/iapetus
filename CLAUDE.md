@AGENTS.md

<!-- Claude Code при наличии CLAUDE.md читает только его, поэтому общий файл
     импортируется. Инструкции не дублировать: импорт грузится целиком. -->

# Claude Code

- Всё по делу — в [AGENTS.md](AGENTS.md), устройство системы — в
  [CONTRACT.md](docs/CONTRACT.md).
- Память проекта — в `.claude/memory/` (задано `autoMemoryDirectory` в
  `.claude/settings.local.json`): план прототипа, открытые вопросы, отвергнутые
  варианты.
- Как Claude Code загружает инструкции, проверяется запуском в каталоге проекта:
  `echo "вопрос" | claude -p --tools ""`.
