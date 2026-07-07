# ADR 0001 — WAL físico de páginas (page-level redo log)

**Status:** Aceito (jul/2026)

## Contexto

O requisito não-funcional nº 1 do projeto é durabilidade absoluta: nenhuma memória
confirmada pode se perder e o arquivo nunca pode ficar irrecuperável, mesmo com
`kill -9` ou queda de energia no meio de uma escrita. O mecanismo de durabilidade
escolhido determina a complexidade do recovery — e recovery é onde bugs de corrupção
moram.

## Decisão

WAL **físico de páginas** (estilo SQLite): toda transação anexa as imagens das páginas
modificadas a um arquivo sidecar (`.mind-wal`), com fsync e commit record com checksum.
Recovery = reaplicar páginas de transações com commit válido e descartar a cauda inválida.
Formato detalhado em [FORMAT.md](../FORMAT.md) §8.

## Alternativas rejeitadas

- **Log lógico de operações** (redo de `remember`/`forget`): mais compacto e flexível,
  mas o replay reexecuta lógica de domínio, multiplicando os estados possíveis pós-crash.
  Verificar isso por fuzzing é muito mais difícil.
- **Copy-on-write / shadow paging** (estilo LMDB): elegante, mas complica o modelo de
  página única com HNSW mutável e o controle fino de fsync no Windows.

## Consequências

- Recovery trivial e verificável mecanicamente (fuzzing do replay em [TESTING.md](../TESTING.md) §3).
- Sidecar transitório é aceitável — SQLite treinou o mercado; arquivo fechado limpo é um único arquivo.
- WAL maior que um log lógico (imagens de página inteiras) — irrelevante para o workload de memória de agente.
